//! Derived group enablement.

use crate::config_yaml::parse_host_port;
use manager_config::{AppConfig, ModelGroup};

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

/// How many of the config's groups are enabled — the group count the
/// status surfaces report (Pete, 2026-10-17, tracker #11). Since
/// v0.1.32 the generated gateway config serves enabled groups only,
/// so the configured total would describe a gateway that is not the
/// one running; the status count is the enabled figure. The
/// configured set stays visible in full on the Routing view's Groups
/// tab.
pub fn enabled_group_count(config: &AppConfig) -> usize {
    config
        .gateway
        .groups
        .iter()
        .filter(|g| group_enabled(g, config))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;
    use manager_config::RoutingStrategy;

    #[test]
    fn model_group_constructs_with_strategy() {
        let g = ModelGroup {
            name: "planner".to_string(),
            members: vec!["planner".to_string()],
            strategy: RoutingStrategy::LatencyBased,
            fallbacks: vec!["coder".to_string(), "openrouter".to_string()],
            purpose: String::new(),
        };
        assert_eq!(g.strategy, RoutingStrategy::LatencyBased);
        assert_eq!(g.fallbacks.len(), 2);
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
    fn enabled_group_count_all_enabled_equals_configured() {
        let cfg = four_backend_config();
        assert!(!cfg.gateway.groups.is_empty());
        assert_eq!(enabled_group_count(&cfg), cfg.gateway.groups.len());
    }

    #[test]
    fn enabled_group_count_counts_mixed_group() {
        let mut cfg = four_backend_config();
        let base = enabled_group_count(&cfg);
        let mut coder_b = backend("coder-b", 8086);
        coder_b.enabled = false;
        cfg.backends.push(coder_b);
        cfg.gateway.groups.push(ModelGroup {
            name: "pair".to_string(),
            members: vec!["coder".to_string(), "coder-b".to_string()],
            strategy: RoutingStrategy::LeastBusy,
            fallbacks: vec![],
            purpose: String::new(),
        });
        // One enabled member is enough: the mixed group counts.
        assert_eq!(enabled_group_count(&cfg), base + 1);
    }

    #[test]
    fn enabled_group_count_skips_all_disabled_group() {
        let mut cfg = four_backend_config();
        let base = enabled_group_count(&cfg);
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .enabled = false;
        // The coder group's only member is disabled: it drops out of
        // the count while the configured total is unchanged.
        assert_eq!(enabled_group_count(&cfg), base - 1);
        assert_eq!(cfg.gateway.groups.len(), base);
    }

    #[test]
    fn enabled_group_count_skips_memberless_group() {
        let mut cfg = four_backend_config();
        let base = enabled_group_count(&cfg);
        cfg.gateway.groups.push(ModelGroup {
            name: "empty".to_string(),
            members: vec![],
            strategy: RoutingStrategy::LeastBusy,
            fallbacks: vec![],
            purpose: String::new(),
        });
        assert_eq!(enabled_group_count(&cfg), base);
    }
}
