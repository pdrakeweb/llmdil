//! Learning-mode activity reports.
//!
//! In Learning mode the runtime blocks-and-records (spec §11.2); the report
//! is parsed here into structured observations. [`LearningReport::summarize`]
//! aggregates observations, and [`suggest_rules`] proposes least-privilege
//! rule additions for activity the current policy does not already allow —
//! the accept/reject review flow that derives the enforced policy.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::decide::{decide_with_env, AccessKind, AccessRequest};
use crate::{FsAccess, FsRule, MxcError, NetRule, Policy, PolicyDecision};

fn default_count() -> u64 {
    1
}

/// One observed decision in a Learning-mode activity report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningEntry {
    pub timestamp: String,
    pub decision: PolicyDecision,
    #[serde(default)]
    pub description: String,
    /// What kind of access was attempted, if recorded.
    #[serde(default)]
    pub kind: Option<AccessKind>,
    /// Filesystem path or hostname, if recorded.
    #[serde(default)]
    pub path_or_host: Option<String>,
    /// Destination port for network observations, if recorded.
    #[serde(default)]
    pub port: Option<u16>,
    /// How many times this was observed (reports may pre-aggregate).
    #[serde(default = "default_count")]
    pub count: u64,
}

/// Activity report produced by a Learning-mode run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningReport {
    pub entries: Vec<LearningEntry>,
}

/// Parse a Learning-mode activity report (JSON) for UI display and for the
/// tighten-the-policy review flow.
pub fn parse_learning_report(json: &str) -> Result<LearningReport, MxcError> {
    Ok(serde_json::from_str(json)?)
}

/// Keep only entries at or after `since_ms` (epoch millis). Entry timestamps
/// are RFC 3339 strings; entries whose timestamp does not parse are kept
/// rather than silently dropped.
pub fn filter_since(report: &LearningReport, since_ms: u64) -> LearningReport {
    let entries = report
        .entries
        .iter()
        .filter(|e| {
            parse_ts_ms(&e.timestamp)
                .map(|ms| ms >= since_ms)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    LearningReport { entries }
}

fn parse_ts_ms(ts: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
}

/// Aggregation key for observations: what was accessed, regardless of when.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObservationKey {
    pub kind: Option<AccessKind>,
    pub path_or_host: Option<String>,
    pub port: Option<u16>,
}

/// Aggregated counts for one distinct observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationSummary {
    pub key: ObservationKey,
    pub total_count: u64,
    pub allows: u64,
    pub prompts: u64,
    pub denies: u64,
    /// Up to 3 sample descriptions, for the review UI.
    pub sample_descriptions: Vec<String>,
}

impl LearningReport {
    /// Aggregate entries by (kind, path_or_host, port), preserving
    /// first-seen order.
    pub fn summarize(&self) -> Vec<ObservationSummary> {
        let mut order: Vec<ObservationKey> = Vec::new();
        let mut map: HashMap<ObservationKey, ObservationSummary> = HashMap::new();
        for e in &self.entries {
            let key = ObservationKey {
                kind: e.kind,
                path_or_host: e.path_or_host.clone(),
                port: e.port,
            };
            let summary = map.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                ObservationSummary {
                    key,
                    total_count: 0,
                    allows: 0,
                    prompts: 0,
                    denies: 0,
                    sample_descriptions: Vec::new(),
                }
            });
            summary.total_count += e.count;
            match e.decision {
                PolicyDecision::Allow => summary.allows += e.count,
                PolicyDecision::Prompt => summary.prompts += e.count,
                PolicyDecision::Deny => summary.denies += e.count,
            }
            if summary.sample_descriptions.len() < 3 && !e.description.is_empty() {
                summary.sample_descriptions.push(e.description.clone());
            }
        }
        order.into_iter().filter_map(|k| map.remove(&k)).collect()
    }
}

/// A proposed rule addition for the tighten-the-policy review flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuleSuggestion {
    AddFsRule {
        rule: FsRule,
        reason: String,
        observed_count: u64,
    },
    AddNetRule {
        rule: NetRule,
        reason: String,
        observed_count: u64,
    },
}

/// Propose least-privilege rule additions for Learning-mode observations the
/// current policy does not already allow. Denied observations ARE proposed —
/// spec §11.2 defines Learning as block + record, so the review flow's job is
/// precisely to decide which denied accesses to allow (accept) or keep denied
/// (reject). Observations without a recorded kind/target are skipped.
/// Uses the process environment for `%VAR%` expansion.
pub fn suggest_rules(report: &LearningReport, policy: &Policy) -> Vec<RuleSuggestion> {
    suggest_rules_with_env(report, policy, &|name| std::env::var(name).ok())
}

/// [`suggest_rules`] with an explicit `%VAR%` resolver (for tests/installers).
pub fn suggest_rules_with_env(
    report: &LearningReport,
    policy: &Policy,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<RuleSuggestion> {
    let mut suggestions = Vec::new();

    // Group file observations by target path: a path seen for both reads and
    // writes yields a single ReadWrite rule, not two rules.
    // (read, write, count, denied_only)
    let mut fs_groups: HashMap<String, (bool, bool, u64, bool)> = HashMap::new();
    let mut fs_order: Vec<String> = Vec::new();
    for summary in report.summarize() {
        let (Some(kind), Some(target)) = (summary.key.kind, summary.key.path_or_host.clone())
        else {
            continue; // no actionable target recorded
        };

        let probe = AccessRequest {
            kind,
            path_or_host: target.clone(),
            port: summary.key.port,
        };
        if decide_with_env(policy, &probe, lookup) == PolicyDecision::Allow {
            continue; // already covered — no new rule needed
        }

        // What the runtime did with these observations, for the review UI:
        // denied accesses are proposed so the user can accept (allow) or
        // reject (stays denied) — spec §11.2 Learning = block + record.
        let outcome = if summary.denies > 0 && summary.allows + summary.prompts == 0 {
            "denied"
        } else {
            "observed"
        };

        match kind {
            AccessKind::FileRead | AccessKind::FileWrite => {
                let denied_only = summary.denies > 0 && summary.allows + summary.prompts == 0;
                let entry = fs_groups.entry(target.clone()).or_insert_with(|| {
                    fs_order.push(target.clone());
                    (false, false, 0, true)
                });
                if kind == AccessKind::FileRead {
                    entry.0 = true;
                } else {
                    entry.1 = true;
                }
                entry.2 += summary.total_count;
                entry.3 &= denied_only;
            }
            AccessKind::Network => {
                let Some(port) = summary.key.port else {
                    continue;
                };
                suggestions.push(RuleSuggestion::AddNetRule {
                    rule: NetRule {
                        host: target,
                        ports: vec![port],
                        decision: PolicyDecision::Allow,
                    },
                    reason: format!(
                        "{outcome} {} time(s) in Learning mode; not covered by the current policy",
                        summary.total_count
                    ),
                    observed_count: summary.total_count,
                });
            }
        }
    }

    for path in fs_order {
        let (read, write, count, denied_only) = fs_groups[&path];
        let access = match (read, write) {
            (true, true) => FsAccess::ReadWrite,
            (true, false) => FsAccess::Read,
            (false, true) => FsAccess::Write,
            (false, false) => continue,
        };
        let outcome = if denied_only { "denied" } else { "observed" };
        suggestions.push(RuleSuggestion::AddFsRule {
            rule: FsRule { path, access },
            reason: format!(
                "{outcome} {count} time(s) in Learning mode; not covered by the current policy"
            ),
            observed_count: count,
        });
    }

    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "entries": [
            {"timestamp": "2026-10-08T10:00:01Z", "decision": "allow",
             "description": "read package.json", "kind": "file_read",
             "path_or_host": "C:\\Users\\Pete\\Projects\\app\\package.json", "count": 12},
            {"timestamp": "2026-10-08T10:00:02Z", "decision": "allow",
             "description": "wrote build output", "kind": "file_write",
             "path_or_host": "D:\\Data\\out\\bundle.js", "count": 3},
            {"timestamp": "2026-10-08T10:00:03Z", "decision": "allow",
             "description": "npm registry", "kind": "network",
             "path_or_host": "registry.npmjs.org", "port": 443, "count": 27},
            {"timestamp": "2026-10-08T10:00:04Z", "decision": "deny",
             "description": "blocked ssh key read", "kind": "file_read",
             "path_or_host": "C:\\Users\\Pete\\.ssh\\id_ed25519", "count": 1},
            {"timestamp": "2026-10-08T10:00:05Z", "decision": "allow",
             "description": "gateway health check", "kind": "network",
             "path_or_host": "127.0.0.1", "port": 4000, "count": 140},
            {"timestamp": "2026-10-08T10:00:06Z", "decision": "allow",
             "description": "no details recorded", "count": 2}
        ]
    }"#;

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn parse_realistic_report() {
        let report = parse_learning_report(FIXTURE).expect("parse");
        assert_eq!(report.entries.len(), 6);
        assert_eq!(report.entries[0].count, 12);
        assert_eq!(report.entries[0].kind, Some(AccessKind::FileRead));
        assert_eq!(report.entries[5].kind, None);
        assert_eq!(report.entries[5].count, 2); // explicit count kept
    }

    #[test]
    fn parse_defaults_count_to_one() {
        let report = parse_learning_report(
            r#"{"entries": [{"timestamp": "t", "decision": "deny", "description": "x"}]}"#,
        )
        .expect("parse");
        assert_eq!(report.entries[0].count, 1);
        assert_eq!(report.entries[0].decision, PolicyDecision::Deny);
    }

    #[test]
    fn parse_invalid_json_errors() {
        let err = parse_learning_report("{not json").unwrap_err();
        assert!(matches!(err, MxcError::Json(_)));
    }

    #[test]
    fn filter_since_keeps_entries_at_or_after_cutoff() {
        let report = parse_learning_report(FIXTURE).expect("parse");
        // 2026-10-08T10:00:04Z in epoch millis.
        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-10-08T10:00:04Z")
            .unwrap()
            .timestamp_millis() as u64;
        let filtered = filter_since(&report, cutoff);
        let stamps: Vec<&str> = filtered
            .entries
            .iter()
            .map(|e| e.timestamp.as_str())
            .collect();
        assert_eq!(
            stamps,
            vec![
                "2026-10-08T10:00:04Z",
                "2026-10-08T10:00:05Z",
                "2026-10-08T10:00:06Z",
            ]
        );
    }

    #[test]
    fn filter_since_keeps_unparseable_timestamps() {
        let report = parse_learning_report(
            r#"{"entries": [
                {"timestamp": "not-a-time", "decision": "deny", "description": "x"},
                {"timestamp": "2026-10-08T10:00:01Z", "decision": "allow", "description": "y"}
            ]}"#,
        )
        .expect("parse");
        let filtered = filter_since(&report, u64::MAX);
        assert_eq!(filtered.entries.len(), 1);
        assert_eq!(filtered.entries[0].timestamp, "not-a-time");
    }

    #[test]
    fn summarize_aggregates_counts() {
        let report = parse_learning_report(FIXTURE).expect("parse");
        let summaries = report.summarize();
        assert_eq!(summaries.len(), 6);
        let pkg = summaries
            .iter()
            .find(|s| {
                s.key.path_or_host.as_deref()
                    == Some("C:\\Users\\Pete\\Projects\\app\\package.json")
            })
            .expect("package.json summary");
        assert_eq!(pkg.total_count, 12);
        assert_eq!(pkg.allows, 12);
        assert_eq!(pkg.denies, 0);
        let ssh = summaries
            .iter()
            .find(|s| s.key.path_or_host.as_deref() == Some("C:\\Users\\Pete\\.ssh\\id_ed25519"))
            .expect("ssh summary");
        assert_eq!(ssh.denies, 1);
    }

    #[test]
    fn suggest_rules_proposes_missing_allows() {
        let report = parse_learning_report(FIXTURE).expect("parse");
        let policy = Policy::default_policy();
        let l = lookup(&[
            ("USERPROFILE", "C:\\Users\\Pete"),
            ("PROGRAMFILES", "C:\\Program Files"),
            ("LOCALAPPDATA", "C:\\Users\\Pete\\AppData\\Local"),
            ("APPDATA", "C:\\Users\\Pete\\AppData\\Roaming"),
        ]);
        let suggestions = suggest_rules_with_env(&report, &policy, &l);

        // D:\Data\out\bundle.js write: not covered → FsRule suggestion
        let fs = suggestions.iter().find_map(|s| match s {
            RuleSuggestion::AddFsRule {
                rule,
                observed_count,
                ..
            } => (rule.path == "D:\\Data\\out\\bundle.js").then_some((rule, observed_count)),
            _ => None,
        });
        let (rule, count) = fs.expect("fs suggestion for bundle.js");
        assert_eq!(rule.access, FsAccess::Write);
        assert_eq!(*count, 3);

        // registry.npmjs.org:443: not covered → NetRule suggestion
        let net = suggestions.iter().find_map(|s| match s {
            RuleSuggestion::AddNetRule {
                rule,
                observed_count,
                ..
            } => (rule.host == "registry.npmjs.org").then_some((rule, observed_count)),
            _ => None,
        });
        let (rule, count) = net.expect("net suggestion for npm registry");
        assert_eq!(rule.ports, vec![443]);
        assert_eq!(*count, 27);

        // package.json read IS covered by the Projects rule → no suggestion
        assert!(!suggestions.iter().any(|s| matches!(
            s,
            RuleSuggestion::AddFsRule { rule, .. } if rule.path.contains("package.json")
        )));
        // 127.0.0.1:4000 IS covered → no suggestion
        assert!(!suggestions.iter().any(|s| matches!(
            s,
            RuleSuggestion::AddNetRule { rule, .. } if rule.host == "127.0.0.1"
        )));
        // denied .ssh read IS proposed (Learning = block + record, spec §11.2):
        // the review flow shows it for accept/reject — the user rejects it
        // and it stays denied.
        let ssh = suggestions.iter().find_map(|s| match s {
            RuleSuggestion::AddFsRule { rule, reason, .. } => {
                (rule.path.contains(".ssh")).then_some((rule, reason))
            }
            _ => None,
        });
        let (rule, reason) = ssh.expect("fs suggestion for denied .ssh read");
        assert_eq!(rule.access, FsAccess::Read);
        assert!(
            reason.contains("denied"),
            "review UI must show it was denied: {reason}"
        );
        // entry without kind/target → no suggestion
        assert_eq!(suggestions.len(), 3);
    }

    #[test]
    fn suggested_rules_validate_and_decide_allow() {
        // The review flow must produce rules that are valid and effective.
        let report = parse_learning_report(FIXTURE).expect("parse");
        let mut policy = Policy::default_policy();
        let l = lookup(&[
            ("USERPROFILE", "C:\\Users\\Pete"),
            ("PROGRAMFILES", "C:\\Program Files"),
            ("LOCALAPPDATA", "C:\\Users\\Pete\\AppData\\Local"),
            ("APPDATA", "C:\\Users\\Pete\\AppData\\Roaming"),
        ]);
        for s in suggest_rules_with_env(&report, &policy, &l) {
            // Simulate the review flow: accept the two legitimate
            // suggestions, reject the .ssh one (it stays denied).
            let accept = match &s {
                RuleSuggestion::AddFsRule { rule, .. } => !rule.path.contains(".ssh"),
                RuleSuggestion::AddNetRule { .. } => true,
            };
            if !accept {
                continue;
            }
            match s {
                RuleSuggestion::AddFsRule { rule, .. } => policy.filesystem.push(rule),
                RuleSuggestion::AddNetRule { rule, .. } => policy.network.push(rule),
            }
        }
        let vr = crate::validate(&policy).expect("validate");
        assert!(vr.valid, "errors: {:?}", vr.errors);
        // the previously-uncovered accesses are now allowed
        let req = AccessRequest {
            kind: AccessKind::FileWrite,
            path_or_host: "D:\\Data\\out\\bundle.js".to_string(),
            port: None,
        };
        assert_eq!(decide_with_env(&policy, &req, &l), PolicyDecision::Allow);
    }
}
