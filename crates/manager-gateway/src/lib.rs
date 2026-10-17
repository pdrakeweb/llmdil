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

use chrono::{DateTime, Utc};
use manager_config::{AppConfig, ModelGroup, RoutingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// LiteLLM model entries
// ---------------------------------------------------------------------------

/// A LiteLLM `model` entry generated from a [`ModelGroup`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiteLlmModelEntry {
    pub model_name: String,
    pub litellm_params: LiteLlmParams,
}

/// The `litellm_params` block: OpenAI-compatible endpoint + routing knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiteLlmParams {
    pub api_base: String,
    pub api_key: String,
    /// Backend-specific strategy hint consumed by the strategy picker.
    pub strategy: RoutingStrategy,
    /// LiteLLM provider/model selector, e.g. `openai/planner` or
    /// `openrouter/qwen/qwen3-235b-a22b`.
    pub model: String,
}

/// OpenRouter fallback target for a group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenRouterTarget {
    pub enabled: bool,
    pub model_allowlist: Vec<String>,
    pub daily_cap_usd: f64,
}

/// Dummy API key sent to local llama-server backends (they ignore it;
/// LiteLLM requires a non-empty value).
pub const LOCAL_API_KEY: &str = "local";

/// Environment-variable reference LiteLLM resolves for the OpenRouter key.
/// The real key lives in Windows Credential Manager and is injected into the
/// LiteLLM process environment at spawn — never written to the YAML.
pub const OPENROUTER_API_KEY_ENV: &str = "os.environ/OPENROUTER_API_KEY";

/// Canonical alias for the OpenRouter fallback deployments.
pub const OPENROUTER_MODEL_NAME: &str = "openrouter";

// ---------------------------------------------------------------------------
// Live backend stats + strategy picking
// ---------------------------------------------------------------------------

/// Point-in-time stats for one backend, as observed by telemetry.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendStats {
    pub id: String,
    /// Whether the backend currently passes health checks.
    pub healthy: bool,
    /// Current request queue depth.
    pub queue_depth: u32,
    /// Rolling p50 time-to-first-token, if latency data is available.
    pub p50_latency_ms: Option<f64>,
    /// Rolling p99 time-to-first-token, if latency data is available.
    pub p99_latency_ms: Option<f64>,
    /// 0.0 (idle) ..= 1.0 (saturated), if load data is available.
    pub load: Option<f64>,
}

impl BackendStats {
    pub fn healthy(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            healthy: true,
            queue_depth: 0,
            p50_latency_ms: None,
            p99_latency_ms: None,
            load: None,
        }
    }
}

/// Strategy picker: resolves the effective strategy for a group given
/// live backend health (e.g. degrade latency-based to simple-shuffle when
/// latency data is unavailable).
pub fn pick_strategy(group: &ModelGroup, latency_data_available: bool) -> RoutingStrategy {
    match group.strategy {
        RoutingStrategy::LatencyBased if !latency_data_available => RoutingStrategy::SimpleShuffle,
        s => s,
    }
}

/// Select a backend id for the next request within a group, honoring the
/// group's effective strategy. Only healthy backends are candidates;
/// returns `None` when no member is healthy (caller then evaluates
/// [`should_fallback`]).
///
/// `seed` drives the simple-shuffle draw via an inline xorshift64 — no
/// external RNG dependency, deterministic per seed for tests.
pub fn pick_backend(
    group: &ModelGroup,
    stats: &[BackendStats],
    latency_data_available: bool,
    seed: u64,
) -> Option<String> {
    let members: Vec<&BackendStats> = stats
        .iter()
        .filter(|s| s.healthy && group.members.iter().any(|m| m == &s.id))
        .collect();
    if members.is_empty() {
        return None;
    }
    let effective = pick_strategy(group, latency_data_available);
    // `members` is non-empty (early return above); the `None` arms below are
    // defensive only — they degrade to shuffle rather than panicking.
    let shuffle = || {
        let idx = (xorshift64(seed) as usize) % members.len();
        members[idx]
    };
    let chosen: &BackendStats = match effective {
        RoutingStrategy::SimpleShuffle => shuffle(),
        RoutingStrategy::LeastBusy => {
            if members.iter().any(|m| m.load.is_some()) {
                match members.iter().min_by(|a, b| {
                    a.load
                        .unwrap_or(f64::INFINITY)
                        .partial_cmp(&b.load.unwrap_or(f64::INFINITY))
                        .unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    Some(best) => best,
                    None => shuffle(),
                }
            } else {
                // No load data: degrade to shuffle rather than mis-route.
                shuffle()
            }
        }
        RoutingStrategy::LatencyBased => {
            // pick_strategy already degraded this when latency data is
            // unavailable, but belt-and-braces if called with stale flags.
            if members.iter().any(|m| m.p50_latency_ms.is_some()) {
                match members.iter().min_by(|a, b| {
                    a.p50_latency_ms
                        .unwrap_or(f64::INFINITY)
                        .partial_cmp(&b.p50_latency_ms.unwrap_or(f64::INFINITY))
                        .unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    Some(best) => best,
                    None => shuffle(),
                }
            } else {
                shuffle()
            }
        }
    };
    Some(chosen.id.clone())
}

fn xorshift64(mut x: u64) -> u64 {
    // Avoid the degenerate all-zero state.
    if x == 0 {
        x = 0x9E37_79B9_7F4A_7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

// ---------------------------------------------------------------------------
// Routing trace + request ring (spec §14 `gateway_test_routing`,
// `gateway_recent_requests`)
// ---------------------------------------------------------------------------

/// One routing decision, as traced and as stored in the request ring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingDecision {
    pub at: DateTime<Utc>,
    pub group: String,
    /// Chosen backend id; `None` when no member was healthy (caller then
    /// evaluates [`should_fallback`]).
    pub backend: Option<String>,
    /// The strategy actually used (after degradation, e.g. latency-based
    /// without latency data runs as simple-shuffle).
    pub strategy: RoutingStrategy,
    /// Human-readable explanation of the decision.
    pub reason: String,
    /// Always `true` for decisions produced by [`trace_routing`]: the
    /// trace is a dry run over caller-supplied health, never a record of
    /// a live request being routed.
    pub dry_run: bool,
}

/// How many decisions the request ring keeps.
pub const ROUTING_LOG_CAP: usize = 50;

/// In-memory ring buffer of recent routing decisions (spec §14
/// `gateway_recent_requests`). Owned by the app; every traced decision is
/// recorded, including dry-run traces from the UI.
#[derive(Debug)]
pub struct RoutingLog {
    entries: VecDeque<RoutingDecision>,
    capacity: usize,
}

impl RoutingLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Record one decision, evicting the oldest when over capacity.
    pub fn record(&mut self, decision: RoutingDecision) {
        self.entries.push_back(decision);
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// Decisions newest-first.
    pub fn recent(&self) -> Vec<RoutingDecision> {
        self.entries.iter().rev().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for RoutingLog {
    fn default() -> Self {
        Self::new(ROUTING_LOG_CAP)
    }
}

/// Trace routing for one request: run [`pick_backend`] and describe the
/// decision. This is an explainer for the gateway view, not the live
/// traffic path (LiteLLM routes live requests). Pure — it does not
/// record into the ring; the caller records the returned decision.
///
/// `stats` is live backend health when available; the UI dry-run passes one
/// healthy stat per group member with no load/latency data, so the trace
/// shows the policy mechanics rather than live state (the reason says so).
pub fn trace_routing(
    group: &ModelGroup,
    stats: &[BackendStats],
    latency_data_available: bool,
    seed: u64,
) -> RoutingDecision {
    let strategy = pick_strategy(group, latency_data_available);
    let backend = pick_backend(group, stats, latency_data_available, seed);
    let healthy: Vec<&str> = stats
        .iter()
        .filter(|s| s.healthy && group.members.iter().any(|m| m == &s.id))
        .map(|s| s.id.as_str())
        .collect();
    let strategy_name = match strategy {
        RoutingStrategy::SimpleShuffle => "simple-shuffle",
        RoutingStrategy::LeastBusy => "least-busy",
        RoutingStrategy::LatencyBased => "latency-based",
    };
    // The basis wording must describe the draw that actually happened:
    // least-busy with no load data (and latency-based with no latency
    // data) silently degrade to a shuffle draw inside pick_backend, so
    // claiming "lowest reported load" would misstate the decision.
    let candidates: Vec<&BackendStats> = stats
        .iter()
        .filter(|s| s.healthy && group.members.iter().any(|m| m == &s.id))
        .collect();
    let reason = match &backend {
        Some(b) => {
            let basis = match strategy {
                RoutingStrategy::SimpleShuffle => format!("shuffle draw (seed {seed})"),
                RoutingStrategy::LeastBusy => {
                    if candidates.iter().any(|m| m.load.is_some()) {
                        "lowest reported load".to_string()
                    } else {
                        format!(
                            "shuffle draw (seed {seed}); least-busy degraded — no load data"
                        )
                    }
                }
                RoutingStrategy::LatencyBased => {
                    if candidates.iter().any(|m| m.p50_latency_ms.is_some()) {
                        "lowest p50 latency".to_string()
                    } else {
                        format!(
                            "shuffle draw (seed {seed}); latency-based degraded — no latency data"
                        )
                    }
                }
            };
            format!(
                "dry-run trace (backends assumed healthy): {strategy_name} -> '{b}' ({basis}); {} healthy member(s) in group '{}'",
                healthy.len(),
                group.name
            )
        }
        None => format!(
            "dry-run trace (backends assumed healthy): no healthy members in group '{}'; OpenRouter fallback would fire (failover)",
            group.name
        ),
    };
    RoutingDecision {
        at: Utc::now(),
        group: group.name.clone(),
        backend,
        strategy,
        reason,
        dry_run: true,
    }
}

// ---------------------------------------------------------------------------
// OpenRouter fallback evaluation (spec §7, three triggers in order)
// ---------------------------------------------------------------------------

/// Thresholds for the overflow trigger.
#[derive(Debug, Clone, PartialEq)]
pub struct OverflowThresholds {
    pub queue_depth: u32,
    pub p99_latency_ms: f64,
    /// Condition must hold this long before overflow fires.
    pub sustained_secs: u64,
}

impl Default for OverflowThresholds {
    fn default() -> Self {
        Self {
            queue_depth: 8,
            p99_latency_ms: 30_000.0,
            sustained_secs: 30,
        }
    }
}

/// Overflow signal as tracked by the app (which owns the duration clock).
#[derive(Debug, Clone, PartialEq)]
pub struct OverflowSignal {
    /// True when queue depth or p99 latency is currently over threshold.
    pub breached: bool,
    /// How long the breach has held, in seconds.
    pub sustained_secs: u64,
}

/// Verdict of the rubric decider (the local planner model at runtime).
#[derive(Debug, Clone, PartialEq)]
pub struct RubricInput {
    /// Task complexity / cost-efficiency score, 0.0..=1.0.
    pub complexity_score: f64,
    /// Escalate when score >= this.
    pub escalate_at_or_above: f64,
}

/// Which OpenRouter trigger fired, in spec §7 order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackTrigger {
    /// (1) All group members unhealthy.
    Failover,
    /// (2) Queue depth or p99 latency over threshold for N seconds.
    Overflow,
    /// (3) Rubric decider routed the task up.
    RubricEscalation,
}

/// Outcome of evaluating the fallback triggers for a group.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackDecision {
    pub trigger: Option<FallbackTrigger>,
    pub reason: String,
}

/// Evaluate the three OpenRouter triggers in spec order.
/// Returns the first trigger that fires; `None` means stay local.
pub fn should_fallback(
    group: &ModelGroup,
    stats: &[BackendStats],
    overflow: &OverflowSignal,
    thresholds: &OverflowThresholds,
    rubric: Option<&RubricInput>,
) -> FallbackDecision {
    // (1) Failover: every member unhealthy (or no stats at all for members).
    let any_healthy = stats
        .iter()
        .any(|s| s.healthy && group.members.iter().any(|m| m == &s.id));
    if !any_healthy {
        return FallbackDecision {
            trigger: Some(FallbackTrigger::Failover),
            reason: format!("all members of group '{}' unhealthy", group.name),
        };
    }
    // (2) Overflow: breach sustained for the configured duration.
    if overflow.breached && overflow.sustained_secs >= thresholds.sustained_secs {
        return FallbackDecision {
            trigger: Some(FallbackTrigger::Overflow),
            reason: format!(
                "queue/latency over threshold for {}s (needs {}s) in group '{}'",
                overflow.sustained_secs, thresholds.sustained_secs, group.name
            ),
        };
    }
    // (3) Rubric-based escalation.
    if let Some(r) = rubric {
        if r.complexity_score >= r.escalate_at_or_above {
            return FallbackDecision {
                trigger: Some(FallbackTrigger::RubricEscalation),
                reason: format!(
                    "rubric score {:.2} >= {:.2} for group '{}'",
                    r.complexity_score, r.escalate_at_or_above, group.name
                ),
            };
        }
    }
    FallbackDecision {
        trigger: None,
        reason: format!("group '{}' served locally", group.name),
    }
}

// ---------------------------------------------------------------------------
// Derived group enablement
// ---------------------------------------------------------------------------

/// Whether one group member counts toward the group's derived enabled
/// state: a member naming a configured backend counts when that backend
/// is enabled; a `host:port` member names an endpoint outside the app's
/// backend list — generation includes it unconditionally, so it always
/// counts. A member naming neither counts for nothing (and, when the
/// group is otherwise enabled, generation rejects it as unknown).
fn member_counts_enabled(member: &str, config: &AppConfig) -> bool {
    match config.backends.iter().find(|b| b.id == member) {
        Some(backend) => backend.enabled,
        None => parse_host_port(member).is_some(),
    }
}

/// How many of the group's members are enabled — the basis of the
/// group's derived state. A group's enabled state is never stored:
/// it is enabled exactly when at least one member backend is enabled
/// (Pete, 2026-10-16), and the generated gateway config includes
/// enabled groups only. This is the one place the rule is computed;
/// the Tauri layer reports these values to the UI rather than
/// re-deriving them.
pub fn group_enabled_member_count(group: &ModelGroup, config: &AppConfig) -> usize {
    group
        .members
        .iter()
        .filter(|m| member_counts_enabled(m, config))
        .count()
}

/// The group's derived enabled state: enabled iff at least one member
/// backend is enabled. See [`group_enabled_member_count`].
pub fn group_enabled(group: &ModelGroup, config: &AppConfig) -> bool {
    group_enabled_member_count(group, config) > 0
}

// ---------------------------------------------------------------------------
// config.yaml generation
// ---------------------------------------------------------------------------

/// Build the `model` entries for one group (members + OpenRouter fallback
/// deployments when the group lists `openrouter` in its fallbacks).
pub fn model_entries_for_group(
    group: &ModelGroup,
    config: &AppConfig,
) -> Result<Vec<LiteLlmModelEntry>, GatewayError> {
    let mut entries = Vec::new();
    let backends: HashMap<&str, &manager_config::BackendConfig> =
        config.backends.iter().map(|b| (b.id.as_str(), b)).collect();

    for member in &group.members {
        if let Some(backend) = backends.get(member.as_str()) {
            if !backend.enabled {
                continue;
            }
            entries.push(LiteLlmModelEntry {
                model_name: group.name.clone(),
                litellm_params: LiteLlmParams {
                    api_base: format!("http://127.0.0.1:{}/v1", backend.port),
                    api_key: LOCAL_API_KEY.to_string(),
                    strategy: group.strategy,
                    model: format!("openai/{}", backend.id),
                },
            });
        } else if let Some((host, port)) = parse_host_port(member) {
            entries.push(LiteLlmModelEntry {
                model_name: group.name.clone(),
                litellm_params: LiteLlmParams {
                    api_base: format!("http://{host}:{port}/v1"),
                    api_key: LOCAL_API_KEY.to_string(),
                    strategy: group.strategy,
                    model: format!("openai/{member}"),
                },
            });
        } else {
            return Err(GatewayError::ConfigInvalid(format!(
                "group '{}' references unknown backend '{member}'",
                group.name
            )));
        }
    }

    // WinML secondary registers under the tool-runner group (spec §6.5).
    if config.winml.enabled && group.name == "tool-runner" {
        entries.push(LiteLlmModelEntry {
            model_name: group.name.clone(),
            litellm_params: LiteLlmParams {
                api_base: format!("http://127.0.0.1:{}/v1", config.winml.port),
                api_key: LOCAL_API_KEY.to_string(),
                strategy: group.strategy,
                model: "openai/winml".to_string(),
            },
        });
    }

    // OpenRouter fallback deployments, one per allowlisted model.
    if group.fallbacks.iter().any(|f| f == OPENROUTER_MODEL_NAME) {
        let cloud = &config.gateway.openrouter;
        if cloud.enabled {
            if cloud.model_allowlist.is_empty() {
                return Err(GatewayError::ConfigInvalid(
                    "openrouter fallback enabled but model_allowlist is empty".to_string(),
                ));
            }
            for model_id in &cloud.model_allowlist {
                entries.push(LiteLlmModelEntry {
                    model_name: OPENROUTER_MODEL_NAME.to_string(),
                    litellm_params: LiteLlmParams {
                        api_base: "https://openrouter.ai/api/v1".to_string(),
                        api_key: OPENROUTER_API_KEY_ENV.to_string(),
                        strategy: RoutingStrategy::SimpleShuffle,
                        model: format!("openrouter/{model_id}"),
                    },
                });
            }
        }
    }

    Ok(entries)
}

/// Parse a `host:port` member reference; `None` when not in that form.
fn parse_host_port(member: &str) -> Option<(&str, u16)> {
    let (host, port) = member.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    port.parse::<u16>().ok().map(|p| (host, p))
}

fn strategy_to_litellm(s: RoutingStrategy) -> &'static str {
    match s {
        RoutingStrategy::SimpleShuffle => "simple-shuffle",
        RoutingStrategy::LeastBusy => "least-busy",
        RoutingStrategy::LatencyBased => "latency-based-routing",
    }
}

/// Generate the full LiteLLM `config.yaml` from the app config.
///
/// The YAML is built with a deterministic string writer (stable key
/// order for the snapshot test). It is written to disk by the Setup
/// Wizard's start-gateway step each time the gateway is (re)started.
/// LiteLLM applies `router_settings.routing_strategy` globally; the
/// emitted value comes from the first group with more than one
/// enabled member's strategy, else the planner group's, else the
/// first group's. Per-group strategies are recorded in the YAML for
/// reference only — LiteLLM routes live traffic under the one global
/// strategy, and [`pick_backend`] serves only the routing-trace
/// explainer.
///
/// Groups are included when they produce deployments: enabled
/// groups (a group's enabled state is derived from its member
/// backends — [`group_enabled`] — never stored), plus groups whose
/// only deployments come from WinML or the OpenRouter fallback.
/// A group with no deployments at all is omitted entirely — its
/// (empty) deployment list and its router fallback rows alike.
pub fn generate_config_yaml(config: &AppConfig) -> Result<String, GatewayError> {
    if config.gateway.groups.is_empty() {
        return Err(GatewayError::ConfigInvalid(
            "no model groups defined".to_string(),
        ));
    }

    // Collect entries per group (validates member references for
    // every group, enabled or not), then keep the groups that
    // produce deployments. That is the enabled groups — plus the
    // two deployment sources that do not come from members: the
    // WinML deployment (registered on the tool-runner group
    // whenever WinML is enabled) and a group's OpenRouter fallback
    // deployments. A group whose members are all disabled but
    // which still yields one of those deployments stays in the
    // config; member-derived enablement alone would omit it and
    // make the deployment unreachable (a v0.1.32 regression — the
    // pre-derivation generator emitted any group whose entries
    // were non-empty).
    let mut group_entries: Vec<(&ModelGroup, Vec<LiteLlmModelEntry>)> = Vec::new();
    for group in &config.gateway.groups {
        let entries = model_entries_for_group(group, config)?;
        if group_enabled(group, config) || !entries.is_empty() {
            group_entries.push((group, entries));
        }
    }
    if group_entries.is_empty() {
        return Err(GatewayError::ConfigInvalid(
            "no enabled model groups (every group's member backends are disabled)".to_string(),
        ));
    }

    let mut out = String::new();
    out.push_str("# Generated by Local LLM Service Manager — DO NOT HAND-EDIT.\n");
    out.push_str("# Regenerated from the app configuration whenever the gateway is (re)started from the Setup Wizard.\n");

    // --- model_list ---
    out.push_str("model_list:\n");
    // Dedupe identical deployments (e.g. the shared OpenRouter fallback
    // entries are produced per group but must appear once).
    let mut emitted: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    for (group, entries) in &group_entries {
        for e in entries {
            let key = (
                e.model_name.clone(),
                e.litellm_params.api_base.clone(),
                e.litellm_params.model.clone(),
            );
            if !emitted.insert(key) {
                continue;
            }
            out.push_str(&format!("  - model_name: {}\n", yaml_scalar(&e.model_name)));
            out.push_str("    litellm_params:\n");
            out.push_str(&format!(
                "      model: {}\n",
                yaml_scalar(&e.litellm_params.model)
            ));
            out.push_str(&format!(
                "      api_base: {}\n",
                yaml_scalar(&e.litellm_params.api_base)
            ));
            out.push_str(&format!(
                "      api_key: {}\n",
                yaml_scalar(&e.litellm_params.api_key)
            ));
            // Record the group's intended strategy alongside the entry so the
            // app's picker and the UI can reconcile it with LiteLLM's global
            // router_settings below. The group name is operator-settable
            // config interpolated into a `#` comment: sanitize it so a
            // name containing newlines cannot inject lines into the
            // generated file.
            if e.model_name == OPENROUTER_MODEL_NAME {
                out.push_str("      # shared OpenRouter fallback deployments\n");
            } else {
                out.push_str(&format!(
                    "      # intended routing strategy for group '{}': {}\n",
                    yaml_comment_text(&group.name),
                    strategy_to_litellm(e.litellm_params.strategy)
                ));
            }
        }
    }

    // --- router_settings (global; see module docs for the resolution rule) ---
    let included_groups: Vec<&ModelGroup> = group_entries.iter().map(|(g, _)| *g).collect();
    let global_strategy = resolve_global_strategy(&included_groups, config);
    out.push_str("router_settings:\n");
    out.push_str(&format!(
        "  routing_strategy: {}\n",
        strategy_to_litellm(global_strategy)
    ));

    // --- litellm_settings ---
    out.push_str("litellm_settings:\n");
    // Prometheus metrics for the app dashboard (Pete approved
    // 2026-10-15): the callback mounts /metrics on the gateway port
    // and maintains the per-deployment success/failure counters the
    // app polls. This REQUIRES the prometheus-client package in the
    // gateway venv — LiteLLM fails to start when the callback is set
    // but the package is missing (verified against 1.105.1); the
    // wizard's install spec carries it.
    out.push_str("  success_callback: [\"prometheus\"]\n");
    out.push_str("  fallbacks:\n");
    let known_names: Vec<&str> = group_entries
        .iter()
        .flat_map(|(_, es)| es.iter().map(|e| e.model_name.as_str()))
        .collect();
    for (group, _) in &group_entries {
        let chain: Vec<&str> = group
            .fallbacks
            .iter()
            .map(String::as_str)
            .filter(|f| {
                // Drop fallbacks with no deployments (e.g. openrouter when
                // the cloud tier is disabled); never emit dangling refs.
                known_names.contains(f) || *f == group.name.as_str()
            })
            .collect();
        if chain.is_empty() {
            continue;
        }
        let rendered = chain
            .iter()
            .map(|f| yaml_scalar(f))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "    - {}: [{}]\n",
            yaml_scalar(&group.name),
            rendered
        ));
    }

    // --- general_settings ---
    // Current LiteLLM refuses to boot without a master key
    // (UnsafeMasterKeyError). The key itself is never written into
    // this file: it is generated by the app, stored in Windows
    // Credential Manager, and injected as LITELLM_MASTER_KEY into the
    // gateway process's environment at spawn; LiteLLM resolves the
    // os.environ/ reference at startup.
    out.push_str("general_settings:\n");
    out.push_str("  master_key: os.environ/LITELLM_MASTER_KEY\n");

    Ok(out)
}

/// Resolve the single global `routing_strategy` for `router_settings`
/// over the groups included in the generated config: first group
/// with more than one *enabled* member's strategy, else the planner
/// group's, else the first group's. "Multi-member" counts enabled
/// members only — a group with one enabled and one disabled member
/// generates a single deployment, and letting it win strategy
/// priority over a genuinely multi-deployment group would take the
/// global strategy from a group that does not have it. The caller
/// guarantees `groups` is non-empty.
fn resolve_global_strategy(groups: &[&ModelGroup], config: &AppConfig) -> RoutingStrategy {
    if let Some(g) = groups
        .iter()
        .find(|g| group_enabled_member_count(g, config) > 1)
    {
        return g.strategy;
    }
    if let Some(g) = groups.iter().find(|g| g.name == "planner") {
        return g.strategy;
    }
    groups[0].strategy
}

/// Minimal YAML scalar quoting: bare when safe, double-quoted otherwise.
/// Sanitize text interpolated into a generated `#` comment: control
/// characters (newlines above all) become spaces, so operator-settable
/// values cannot break out of the comment and inject lines into the
/// generated config.
fn yaml_comment_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Minimal YAML scalar quoting: bare when safe, double-quoted otherwise.
fn yaml_scalar(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'))
        && !s.contains("://")
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    if safe {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

// ---------------------------------------------------------------------------
// Config diff (library utility; no UI flow currently calls it)
// ---------------------------------------------------------------------------

/// One line of a config diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Added,
    Removed,
}

/// One line of a config diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Line-based diff of two YAML documents (old vs new), via LCS.
/// Small inputs (configs); O(n*m) is fine. Library utility: currently
/// unwired — no command or UI flow calls it.
pub fn diff_config_yaml(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let n = a.len();
    let m = b.len();

    // LCS length table.
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine {
                kind: DiffKind::Same,
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(DiffLine {
                kind: DiffKind::Removed,
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                kind: DiffKind::Added,
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine {
            kind: DiffKind::Removed,
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < m {
        out.push(DiffLine {
            kind: DiffKind::Added,
            text: b[j].to_string(),
        });
        j += 1;
    }
    out
}

/// (added, removed) line counts for a diff.
pub fn diff_summary(lines: &[DiffLine]) -> (usize, usize) {
    let added = lines.iter().filter(|l| l.kind == DiffKind::Added).count();
    let removed = lines.iter().filter(|l| l.kind == DiffKind::Removed).count();
    (added, removed)
}

/// Render a diff in unified-ish `+`/`-`/` ` form for the review UI.
pub fn render_unified_diff(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for l in lines {
        let prefix = match l.kind {
            DiffKind::Same => " ",
            DiffKind::Added => "+",
            DiffKind::Removed => "-",
        };
        out.push_str(prefix);
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

/// Gateway errors.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("invalid gateway config: {0}")]
    ConfigInvalid(String),
    #[error("YAML serialization failed: {0}")]
    Serialization(#[from] serde_yaml::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_config::{BackendConfig, CloudTierConfig, ServerFlags, SplitMode};
    use std::path::PathBuf;

    fn backend(id: &str, port: u16) -> BackendConfig {
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
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            tool_call_probe: None,
        }
    }

    /// The 4-backend stack from the spec: planner :8081, coder :8082,
    /// coder-fast :8083, tool-runner :8084, OpenRouter tier enabled.
    fn four_backend_config() -> AppConfig {
        let mut cfg = AppConfig::default_config();
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

    fn group(name: &str, strategy: RoutingStrategy, members: &[&str]) -> ModelGroup {
        ModelGroup {
            name: name.to_string(),
            members: members.iter().map(|s| s.to_string()).collect(),
            strategy,
            fallbacks: vec![],
        }
    }

    fn stats(id: &str, healthy: bool, load: Option<f64>, p50: Option<f64>) -> BackendStats {
        BackendStats {
            id: id.to_string(),
            healthy,
            queue_depth: 0,
            p50_latency_ms: p50,
            p99_latency_ms: p50.map(|p| p * 1.8),
            load,
        }
    }

    #[test]
    fn model_group_constructs_with_strategy() {
        let g = ModelGroup {
            name: "planner".to_string(),
            members: vec!["planner".to_string()],
            strategy: RoutingStrategy::LatencyBased,
            fallbacks: vec!["coder".to_string(), "openrouter".to_string()],
        };
        assert_eq!(g.strategy, RoutingStrategy::LatencyBased);
        assert_eq!(g.fallbacks.len(), 2);
    }

    #[test]
    fn yaml_snapshot_matches_checked_in_file() {
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        let expected =
            std::fs::read_to_string("tests/expected-config.yaml").expect("read snapshot");
        assert_eq!(yaml, expected, "generated YAML drifted from snapshot");
    }

    #[test]
    fn yaml_group_name_cannot_inject_comment_lines() {
        // Group names are operator-settable and land in a `#` comment;
        // an embedded newline must not inject lines into the file.
        let mut cfg = four_backend_config();
        cfg.gateway.groups[0].name = "planner\nmalicious: true".to_string();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            !yaml.lines().any(|l| l == "malicious: true"),
            "injected line present:\n{yaml}"
        );
        assert!(yaml.contains("group 'planner malicious: true'"));
    }

    #[test]
    fn yaml_contains_all_backends_groups_and_fallbacks() {
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        for port in [8081, 8082, 8083, 8084] {
            assert!(
                yaml.contains(&format!("http://127.0.0.1:{port}/v1")),
                "missing backend :{port}"
            );
        }
        for name in [
            "planner",
            "coder-fast",
            "coder",
            "tool-runner",
            "openrouter",
        ] {
            assert!(
                yaml.contains(&format!("model_name: {name}")),
                "missing {name}"
            );
        }
        assert!(yaml.contains("routing_strategy: latency-based-routing"));
        assert!(yaml.contains("- planner: [coder, openrouter]"));
        assert!(yaml.contains("- coder-fast: [coder, openrouter]"));
        assert!(yaml.contains("- coder: [openrouter]"));
        assert!(yaml.contains("- tool-runner: [openrouter]"));
        assert!(yaml.contains("os.environ/OPENROUTER_API_KEY"));
        assert!(!yaml.contains("sk-"), "no real secrets in generated YAML");
    }

    #[test]
    fn yaml_drops_openrouter_when_tier_disabled() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.enabled = false;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("openrouter.ai"));
        assert!(!yaml.contains("model_name: openrouter"));
        // Fallback chains must not dangle.
        assert!(!yaml.contains("openrouter]"));
        assert!(!yaml.contains(", openrouter"));
    }

    #[test]
    fn group_enablement_is_derived_from_member_backends() {
        let mut cfg = four_backend_config();
        // All members enabled by construction.
        for g in &cfg.gateway.groups {
            assert!(group_enabled(g, &cfg), "group '{}'", g.name);
            assert_eq!(group_enabled_member_count(g, &cfg), 1);
        }
        // Disabling the only member disables the group.
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .enabled = false;
        let coder = cfg
            .gateway
            .groups
            .iter()
            .find(|g| g.name == "coder")
            .unwrap();
        assert!(!group_enabled(coder, &cfg));
        assert_eq!(group_enabled_member_count(coder, &cfg), 0);
        // Other groups are unaffected.
        let planner = cfg
            .gateway
            .groups
            .iter()
            .find(|g| g.name == "planner")
            .unwrap();
        assert!(group_enabled(planner, &cfg));
    }

    #[test]
    fn yaml_omits_group_with_all_members_disabled() {
        let mut cfg = four_backend_config();
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .enabled = false;
        // The coder group must have no deployments at all to be
        // omitted: strip its OpenRouter fallback (a group whose
        // fallback deployments survive is kept — see
        // yaml_keeps_openrouter_only_group_when_members_disabled).
        cfg.gateway
            .groups
            .iter_mut()
            .find(|g| g.name == "coder")
            .unwrap()
            .fallbacks = vec![];
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // No deployment and no fallback row for the disabled group.
        assert!(
            !yaml.lines().any(|l| l.trim() == "- model_name: coder"),
            "yaml:\n{yaml}"
        );
        assert!(!yaml.contains("127.0.0.1:8082"), "yaml:\n{yaml}");
        assert!(
            !yaml.lines().any(|l| l.trim().starts_with("- coder:")),
            "yaml:\n{yaml}"
        );
        // Fallback chains naming it drop the reference instead of
        // leaving it dangling.
        assert!(yaml.contains("- planner: [openrouter]"), "yaml:\n{yaml}");
        assert!(yaml.contains("- coder-fast: [openrouter]"), "yaml:\n{yaml}");
        // Remaining groups are generated as before.
        assert!(yaml.contains("model_name: planner"));
        assert!(yaml.contains("http://127.0.0.1:8081/v1"));
    }

    #[test]
    fn yaml_mixed_group_includes_only_enabled_members() {
        let mut cfg = four_backend_config();
        let mut coder_b = backend("coder-b", 8086);
        coder_b.enabled = false;
        cfg.backends.push(coder_b);
        cfg.gateway.groups.push(ModelGroup {
            name: "pair".to_string(),
            members: vec!["coder".to_string(), "coder-b".to_string()],
            strategy: RoutingStrategy::LeastBusy,
            fallbacks: vec![],
        });
        let pair = cfg
            .gateway
            .groups
            .iter()
            .find(|g| g.name == "pair")
            .unwrap();
        assert!(group_enabled(pair, &cfg));
        assert_eq!(group_enabled_member_count(pair, &cfg), 1);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // The group is enabled through its one enabled member: present,
        // with only that member's deployment.
        assert!(yaml.contains("model_name: pair"), "yaml:\n{yaml}");
        assert!(!yaml.contains("8086"), "disabled member leaked:\n{yaml}");
    }

    #[test]
    fn yaml_errors_when_every_group_is_disabled() {
        let mut cfg = four_backend_config();
        for b in &mut cfg.backends {
            b.enabled = false;
        }
        // The cloud tier off as well: with it on, the groups'
        // OpenRouter fallback deployments would still generate
        // (yaml_keeps_openrouter_only_group_when_members_disabled).
        cfg.gateway.openrouter.enabled = false;
        let err = generate_config_yaml(&cfg).unwrap_err();
        assert!(matches!(err, GatewayError::ConfigInvalid(_)));
        assert!(err.to_string().contains("no enabled model groups"), "{err}");
    }

    #[test]
    fn yaml_rejects_unknown_backend_member() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups[0].members = vec!["nope".to_string()];
        let err = generate_config_yaml(&cfg).unwrap_err();
        assert!(matches!(err, GatewayError::ConfigInvalid(_)));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn yaml_rejects_empty_groups() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups.clear();
        assert!(matches!(
            generate_config_yaml(&cfg).unwrap_err(),
            GatewayError::ConfigInvalid(_)
        ));
    }

    #[test]
    fn yaml_rejects_openrouter_enabled_with_empty_allowlist() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.model_allowlist.clear();
        assert!(matches!(
            generate_config_yaml(&cfg).unwrap_err(),
            GatewayError::ConfigInvalid(_)
        ));
    }

    #[test]
    fn yaml_accepts_host_port_member_refs() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups.push(ModelGroup {
            name: "aux".to_string(),
            members: vec!["127.0.0.1:8099".to_string()],
            strategy: RoutingStrategy::SimpleShuffle,
            fallbacks: vec![],
        });
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("http://127.0.0.1:8099/v1"));
    }

    #[test]
    fn yaml_keeps_winml_group_when_its_member_backend_is_disabled() {
        // WinML registers under tool-runner whenever WinML is
        // enabled; the deployment does not come from the group's
        // member backend, so disabling that backend must not drop
        // the group (and WinML with it) from the generated config.
        let mut cfg = four_backend_config();
        for b in cfg.backends.iter_mut() {
            if b.id == "tool-runner" {
                b.enabled = false;
            }
        }
        cfg.winml.enabled = true;
        cfg.winml.port = 8090;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains("http://127.0.0.1:8090/v1"),
            "winml deployment survives its group having no enabled member:\n{yaml}"
        );
    }

    #[test]
    fn yaml_keeps_openrouter_only_group_when_members_disabled() {
        // A group whose only deployments are its OpenRouter
        // fallback entries is the only group: generation must not
        // fail ConfigInvalid just because no member is enabled.
        let mut cfg = four_backend_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = false;
        }
        cfg.gateway.groups = vec![ModelGroup {
            name: "cloud".to_string(),
            members: vec!["coder".to_string()],
            strategy: RoutingStrategy::SimpleShuffle,
            fallbacks: vec!["openrouter".to_string()],
        }];
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("openrouter.ai"), "{yaml}");
        // With the cloud tier off too, the group has no deployments
        // at all and generation fails closed, as before.
        cfg.gateway.openrouter.enabled = false;
        assert!(matches!(
            generate_config_yaml(&cfg),
            Err(GatewayError::ConfigInvalid(_))
        ));
    }

    #[test]
    fn global_strategy_counts_enabled_members_only() {
        // Group "mixed" has two members but only one enabled; group
        // "pair" has two enabled. The global strategy must come
        // from "pair" — "mixed" generates a single deployment.
        let mut cfg = four_backend_config();
        cfg.gateway.groups = vec![
            ModelGroup {
                name: "mixed".to_string(),
                members: vec!["planner".to_string(), "coder-fast".to_string()],
                strategy: RoutingStrategy::LeastBusy,
                fallbacks: vec![],
            },
            ModelGroup {
                name: "pair".to_string(),
                members: vec!["coder".to_string(), "tool-runner".to_string()],
                strategy: RoutingStrategy::LatencyBased,
                fallbacks: vec![],
            },
        ];
        for b in cfg.backends.iter_mut() {
            if b.id == "coder-fast" {
                b.enabled = false;
            }
        }
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains("routing_strategy: latency-based-routing"),
            "{yaml}"
        );
    }

    #[test]
    fn yaml_includes_winml_under_tool_runner_when_registered() {
        let mut cfg = four_backend_config();
        cfg.winml.enabled = true;
        cfg.winml.port = 8090;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("http://127.0.0.1:8090/v1"));
        // Only under tool-runner, not planner.
        let planner_section = yaml
            .split("model_name: planner")
            .nth(1)
            .unwrap()
            .split("model_name: ")
            .next()
            .unwrap();
        assert!(!planner_section.contains(":8090"));
    }

    #[test]
    fn pick_strategy_degrades_latency_based_without_data() {
        let g = group("planner", RoutingStrategy::LatencyBased, &["planner"]);
        assert_eq!(
            pick_strategy(&g, false),
            RoutingStrategy::SimpleShuffle,
            "latency-based degrades to shuffle without latency data"
        );
        assert_eq!(
            pick_strategy(&g, true),
            RoutingStrategy::LatencyBased,
            "latency-based kept when data available"
        );
        let g2 = group("coder-fast", RoutingStrategy::LeastBusy, &["coder-fast"]);
        assert_eq!(pick_strategy(&g2, false), RoutingStrategy::LeastBusy);
    }

    #[test]
    fn least_busy_picks_lowest_load() {
        let g = group("coder-fast", RoutingStrategy::LeastBusy, &["a", "b", "c"]);
        let s = vec![
            stats("a", true, Some(0.8), None),
            stats("b", true, Some(0.2), None),
            stats("c", true, Some(0.5), None),
        ];
        assert_eq!(pick_backend(&g, &s, false, 42).as_deref(), Some("b"));
    }

    #[test]
    fn latency_based_picks_lowest_p50() {
        let g = group("planner", RoutingStrategy::LatencyBased, &["a", "b"]);
        let s = vec![
            stats("a", true, None, Some(900.0)),
            stats("b", true, None, Some(250.0)),
        ];
        assert_eq!(pick_backend(&g, &s, true, 7).as_deref(), Some("b"));
    }

    #[test]
    fn shuffle_distributes_across_members() {
        let g = group("coder", RoutingStrategy::SimpleShuffle, &["a", "b", "c"]);
        let s = vec![
            stats("a", true, None, None),
            stats("b", true, None, None),
            stats("c", true, None, None),
        ];
        let mut seen = std::collections::HashSet::new();
        for seed in 0..60u64 {
            seen.insert(pick_backend(&g, &s, false, seed).unwrap());
        }
        assert_eq!(seen.len(), 3, "shuffle should reach every member");
    }

    #[test]
    fn picker_skips_unhealthy_and_returns_none_when_all_down() {
        let g = group("coder", RoutingStrategy::LeastBusy, &["a", "b"]);
        let s = vec![
            stats("a", false, Some(0.0), None),
            stats("b", true, Some(0.9), None),
        ];
        assert_eq!(pick_backend(&g, &s, false, 1).as_deref(), Some("b"));
        let all_down = vec![
            stats("a", false, Some(0.0), None),
            stats("b", false, Some(0.0), None),
        ];
        assert_eq!(pick_backend(&g, &all_down, false, 1), None);
    }

    #[test]
    fn least_busy_degrades_to_shuffle_without_load_data() {
        let g = group("coder-fast", RoutingStrategy::LeastBusy, &["a", "b"]);
        let s = vec![stats("a", true, None, None), stats("b", true, None, None)];
        // Must still pick something (shuffle fallback), never panic.
        assert!(pick_backend(&g, &s, false, 3).is_some());
    }

    #[test]
    fn fallback_failover_when_all_members_down() {
        let g = group("planner", RoutingStrategy::LatencyBased, &["planner"]);
        let s = vec![stats("planner", false, None, None)];
        let d = should_fallback(
            &g,
            &s,
            &OverflowSignal {
                breached: false,
                sustained_secs: 0,
            },
            &OverflowThresholds::default(),
            None,
        );
        assert_eq!(d.trigger, Some(FallbackTrigger::Failover));
    }

    #[test]
    fn fallback_overflow_when_sustained() {
        let g = group("coder", RoutingStrategy::SimpleShuffle, &["coder"]);
        let s = vec![stats("coder", true, Some(0.9), Some(1000.0))];
        let thresholds = OverflowThresholds {
            sustained_secs: 30,
            ..Default::default()
        };
        // Breach not yet sustained: no trigger.
        let d = should_fallback(
            &g,
            &s,
            &OverflowSignal {
                breached: true,
                sustained_secs: 10,
            },
            &thresholds,
            None,
        );
        assert_eq!(d.trigger, None);
        // Sustained: overflow fires.
        let d = should_fallback(
            &g,
            &s,
            &OverflowSignal {
                breached: true,
                sustained_secs: 45,
            },
            &thresholds,
            None,
        );
        assert_eq!(d.trigger, Some(FallbackTrigger::Overflow));
    }

    #[test]
    fn fallback_rubric_escalation() {
        let g = group("planner", RoutingStrategy::LatencyBased, &["planner"]);
        let s = vec![stats("planner", true, Some(0.3), Some(400.0))];
        let rubric = RubricInput {
            complexity_score: 0.85,
            escalate_at_or_above: 0.8,
        };
        let d = should_fallback(
            &g,
            &s,
            &OverflowSignal {
                breached: false,
                sustained_secs: 0,
            },
            &OverflowThresholds::default(),
            Some(&rubric),
        );
        assert_eq!(d.trigger, Some(FallbackTrigger::RubricEscalation));
        // Below threshold: stay local.
        let low = RubricInput {
            complexity_score: 0.2,
            ..rubric
        };
        let d = should_fallback(
            &g,
            &s,
            &OverflowSignal {
                breached: false,
                sustained_secs: 0,
            },
            &OverflowThresholds::default(),
            Some(&low),
        );
        assert_eq!(d.trigger, None);
    }

    #[test]
    fn fallback_trigger_order_failover_first() {
        let g = group("planner", RoutingStrategy::LatencyBased, &["planner"]);
        let s = vec![stats("planner", false, None, None)];
        let d = should_fallback(
            &g,
            &s,
            &OverflowSignal {
                breached: true,
                sustained_secs: 999,
            },
            &OverflowThresholds::default(),
            Some(&RubricInput {
                complexity_score: 1.0,
                escalate_at_or_above: 0.0,
            }),
        );
        assert_eq!(
            d.trigger,
            Some(FallbackTrigger::Failover),
            "failover wins over overflow and rubric"
        );
    }

    #[test]
    fn diff_identical_is_all_same() {
        let doc = "a: 1\nb: 2\n";
        let lines = diff_config_yaml(doc, doc);
        assert!(lines.iter().all(|l| l.kind == DiffKind::Same));
        assert_eq!(diff_summary(&lines), (0, 0));
    }

    #[test]
    fn diff_detects_changed_port() {
        let old = "model_list:\n  - api_base: http://127.0.0.1:8081/v1\n";
        let new = "model_list:\n  - api_base: http://127.0.0.1:8082/v1\n";
        let lines = diff_config_yaml(old, new);
        let (added, removed) = diff_summary(&lines);
        assert_eq!((added, removed), (1, 1));
        let rendered = render_unified_diff(&lines);
        assert!(rendered.contains("-  - api_base: http://127.0.0.1:8081/v1"));
        assert!(rendered.contains("+  - api_base: http://127.0.0.1:8082/v1"));
        assert!(rendered.contains(" model_list:"));
    }

    #[test]
    fn diff_detects_appended_block() {
        let old = "a: 1\n";
        let new = "a: 1\nb: 2\nc: 3\n";
        let (added, removed) = diff_summary(&diff_config_yaml(old, new));
        assert_eq!((added, removed), (2, 0));
    }

    #[test]
    fn openrouter_target_type_round_trips() {
        let t = OpenRouterTarget {
            enabled: true,
            model_allowlist: vec!["qwen/qwen3-235b-a22b".to_string()],
            daily_cap_usd: 5.0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: OpenRouterTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn trace_routing_reports_effective_strategy_and_reason() {
        let g = group("coder-fast", RoutingStrategy::LeastBusy, &["a", "b"]);
        let s = vec![
            stats("a", true, Some(0.8), None),
            stats("b", true, Some(0.2), None),
        ];
        let d = trace_routing(&g, &s, false, 1);
        assert_eq!(d.group, "coder-fast");
        assert_eq!(d.strategy, RoutingStrategy::LeastBusy);
        assert_eq!(d.backend.as_deref(), Some("b"));
        assert!(d.reason.contains("least-busy"), "reason: {}", d.reason);
        assert!(d.reason.contains("'b'"), "reason: {}", d.reason);
    }

    #[test]
    fn trace_routing_reports_degraded_strategy() {
        // Latency-based without latency data degrades to simple-shuffle;
        // the trace must report what actually ran.
        let g = group("planner", RoutingStrategy::LatencyBased, &["a", "b"]);
        let s = vec![stats("a", true, None, None), stats("b", true, None, None)];
        let d = trace_routing(&g, &s, false, 1);
        assert_eq!(d.strategy, RoutingStrategy::SimpleShuffle);
        assert!(d.reason.contains("simple-shuffle"), "reason: {}", d.reason);
        assert!(d.backend.is_some());
    }

    #[test]
    fn trace_routing_none_when_all_down() {
        let g = group("coder", RoutingStrategy::LeastBusy, &["a"]);
        let s = vec![stats("a", false, None, None)];
        let d = trace_routing(&g, &s, false, 1);
        assert_eq!(d.backend, None);
        assert!(
            d.reason.contains("no healthy members"),
            "reason: {}",
            d.reason
        );
        assert!(d.reason.contains("failover"), "reason: {}", d.reason);
    }

    #[test]
    fn routing_log_is_bounded_and_newest_first() {
        let mut log = RoutingLog::new(3);
        for i in 0..5u64 {
            let g = group("coder", RoutingStrategy::SimpleShuffle, &["a"]);
            let s = vec![stats("a", true, None, None)];
            let mut d = trace_routing(&g, &s, false, i);
            d.group = format!("g{i}");
            log.record(d);
        }
        assert_eq!(log.len(), 3);
        let recent = log.recent();
        assert_eq!(recent[0].group, "g4", "newest first");
        assert_eq!(recent[2].group, "g2", "oldest evicted");
        log.clear();
        assert!(log.is_empty());
    }

    #[test]
    fn routing_decision_serializes_for_ui() {
        let g = group("planner", RoutingStrategy::LatencyBased, &["a"]);
        let s = vec![stats("a", true, None, Some(100.0))];
        let d = trace_routing(&g, &s, true, 9);
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["group"], "planner");
        assert_eq!(json["backend"], "a");
        assert_eq!(json["strategy"], "latency-based");
        assert!(json["at"].is_string());
        assert!(json["reason"].is_string());
    }
}
