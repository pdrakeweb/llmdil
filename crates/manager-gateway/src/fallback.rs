//! OpenRouter fallback evaluation (spec §7).

use crate::*;
use manager_config::{AppConfig, ModelGroup};

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

impl OverflowThresholds {
    /// The thresholds configured on the cloud tier.
    pub fn from_config(c: &manager_config::OverflowEscalationConfig) -> Self {
        Self {
            queue_depth: c.queue_depth,
            p99_latency_ms: f64::from(c.p99_latency_ms),
            sustained_secs: u64::from(c.sustained_secs),
        }
    }

    /// Whether a group's instantaneous readings breach the overflow
    /// thresholds: in-flight at or above the queue depth, or p99 at
    /// or above the latency bound. The generated routing callback
    /// applies this exact rule at request time
    /// (`_overflow_breached`); the parity test pins the two.
    pub fn breached(&self, in_flight: u32, p99_latency_ms: Option<f64>) -> bool {
        in_flight >= self.queue_depth || p99_latency_ms.is_some_and(|p| p >= self.p99_latency_ms)
    }
}

/// The overflow-escalation table baked into the generated routing
/// callback (spec §7 trigger 2): the thresholds and the groups whose
/// new requests may escalate to the cloud tier.
#[derive(Debug, Clone, PartialEq)]
pub struct OverflowEscalationTable {
    pub thresholds: OverflowThresholds,
    /// Groups in the generated config that list `openrouter` in their
    /// fallback chain and serve local deployments, in config order.
    pub groups: Vec<String>,
}

/// The overflow-escalation table for a config, or `None` when the
/// trigger is off: escalation disabled, or no OpenRouter deployments
/// to escalate to (tier disabled, zero cap, empty allowlist), or no
/// group lists `openrouter` as a fallback. Only groups that opt into
/// the cloud tier through their own fallback chain escalate — the
/// trigger never routes a group to OpenRouter that failover would not.
pub fn overflow_escalation_table(config: &AppConfig) -> Option<OverflowEscalationTable> {
    let cloud = &config.gateway.openrouter;
    if !cloud.overflow.enabled || !openrouter_tier_active(config) {
        return None;
    }
    let groups: Vec<String> = config
        .gateway
        .groups
        .iter()
        .filter(|g| g.name != OPENROUTER_MODEL_NAME)
        .filter(|g| g.fallbacks.iter().any(|f| f == OPENROUTER_MODEL_NAME))
        .filter(|g| {
            model_entries_for_group(g, config)
                .map(|es| es.iter().any(|e| e.model_name == g.name))
                .unwrap_or(false)
        })
        .map(|g| g.name.clone())
        .collect();
    if groups.is_empty() {
        return None;
    }
    Some(OverflowEscalationTable {
        thresholds: OverflowThresholds::from_config(&cloud.overflow),
        groups,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;
    use manager_config::RoutingStrategy;

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
}
