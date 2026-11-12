//! Live backend stats, strategy picking, and the routing trace/request ring.

use chrono::{DateTime, Utc};
use manager_config::{ModelGroup, RoutingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;

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
