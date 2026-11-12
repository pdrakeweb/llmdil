//! OpenRouter fallback evaluation (spec §7).

use crate::*;
use manager_config::ModelGroup;

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
