//! Plan document types (design §4, §6).
//!
//! A plan is the parsed result of one consultation: a list of items,
//! each a structured change kind (or advice), a tri-state decision,
//! and — after an apply run — a per-item outcome. The plan persists
//! server-side (`<data_dir>/optimize-plan.json`) so an unapplied plan
//! and the operator's decisions survive an app restart.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The model's self-reported confidence. Anything unrecognized in
/// the raw response parses as `Low` (design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn parse(s: &str) -> Self {
        match s {
            "high" => Confidence::High,
            "medium" => Confidence::Medium,
            _ => Confidence::Low,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

/// The operator's tri-state decision on one item. Only `Accepted`
/// items ever execute; `Undecided` does not (design §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Undecided,
    Accepted,
    Denied,
}

/// The closed set of change kinds (design §4). Kinds outside this
/// set never execute: unknown kinds are dropped at parse, and known
/// but unmapped families (gateway port, downloads, wizard actions)
/// are demoted to `Advice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanKind {
    #[serde(rename = "backend.create")]
    BackendCreate,
    #[serde(rename = "backend.delete")]
    BackendDelete,
    #[serde(rename = "backend.set_context")]
    BackendSetContext,
    #[serde(rename = "backend.set_flags")]
    BackendSetFlags,
    #[serde(rename = "backend.set_offload")]
    BackendSetOffload,
    #[serde(rename = "backend.set_placement")]
    BackendSetPlacement,
    #[serde(rename = "backend.set_model")]
    BackendSetModel,
    #[serde(rename = "backend.set_enabled")]
    BackendSetEnabled,
    #[serde(rename = "group.set_members")]
    GroupSetMembers,
    #[serde(rename = "group.set_strategy")]
    GroupSetStrategy,
    #[serde(rename = "group.set_fallbacks")]
    GroupSetFallbacks,
    #[serde(rename = "group.create")]
    GroupCreate,
    #[serde(rename = "group.delete")]
    GroupDelete,
    #[serde(rename = "model.add")]
    ModelAdd,
    #[serde(rename = "advice")]
    Advice,
}

impl PlanKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanKind::BackendCreate => "backend.create",
            PlanKind::BackendDelete => "backend.delete",
            PlanKind::BackendSetContext => "backend.set_context",
            PlanKind::BackendSetFlags => "backend.set_flags",
            PlanKind::BackendSetOffload => "backend.set_offload",
            PlanKind::BackendSetPlacement => "backend.set_placement",
            PlanKind::BackendSetModel => "backend.set_model",
            PlanKind::BackendSetEnabled => "backend.set_enabled",
            PlanKind::GroupSetMembers => "group.set_members",
            PlanKind::GroupSetStrategy => "group.set_strategy",
            PlanKind::GroupSetFallbacks => "group.set_fallbacks",
            PlanKind::GroupCreate => "group.create",
            PlanKind::GroupDelete => "group.delete",
            PlanKind::ModelAdd => "model.add",
            PlanKind::Advice => "advice",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "backend.create" => PlanKind::BackendCreate,
            "backend.delete" => PlanKind::BackendDelete,
            "backend.set_context" => PlanKind::BackendSetContext,
            "backend.set_flags" => PlanKind::BackendSetFlags,
            "backend.set_offload" => PlanKind::BackendSetOffload,
            "backend.set_placement" => PlanKind::BackendSetPlacement,
            "backend.set_model" => PlanKind::BackendSetModel,
            "backend.set_enabled" => PlanKind::BackendSetEnabled,
            "group.set_members" => PlanKind::GroupSetMembers,
            "group.set_strategy" => PlanKind::GroupSetStrategy,
            "group.set_fallbacks" => PlanKind::GroupSetFallbacks,
            "group.create" => PlanKind::GroupCreate,
            "group.delete" => PlanKind::GroupDelete,
            "model.add" => PlanKind::ModelAdd,
            "advice" => PlanKind::Advice,
            _ => return None,
        })
    }

    /// Short chip label for the plan row (design §1.3).
    pub fn short_label(&self) -> &'static str {
        match self {
            PlanKind::BackendCreate => "New backend",
            PlanKind::BackendDelete => "Delete",
            PlanKind::BackendSetContext => "Context",
            PlanKind::BackendSetFlags => "Flags",
            PlanKind::BackendSetOffload => "Offload",
            PlanKind::BackendSetPlacement => "Placement",
            PlanKind::BackendSetModel => "Model",
            PlanKind::BackendSetEnabled => "Enabled",
            PlanKind::GroupSetMembers
            | PlanKind::GroupSetStrategy
            | PlanKind::GroupSetFallbacks => "Routing",
            PlanKind::GroupCreate => "New group",
            PlanKind::GroupDelete => "Delete",
            PlanKind::ModelAdd => "New model",
            PlanKind::Advice => "Advice",
        }
    }

    /// Destructive items start denied (design §6) and are gated
    /// separately at apply (design §5.4).
    pub fn is_destructive(&self) -> bool {
        matches!(self, PlanKind::BackendDelete | PlanKind::GroupDelete)
    }

    pub fn is_advice(&self) -> bool {
        matches!(self, PlanKind::Advice)
    }
}

/// Outcome of one item from an apply run (design §5.2). Set by the
/// UI orchestrator as execution proceeds; execution halts on the
/// first failure, so later accepted items are `NotAttempted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcome {
    Applied,
    Failed,
    NotAttempted,
}

/// One plan item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    /// 1-based position in the plan, assigned at parse (design §4).
    pub id: usize,
    /// Hash of kind + target + change; denial memory keys off it
    /// across consultations (design §6).
    pub fingerprint: String,
    pub kind: PlanKind,
    /// Backend id, group name, or model id the item acts on. For
    /// creates, the new entity's id/name. `None` only for advice
    /// items that name no target.
    pub target: Option<String>,
    /// The normalized change payload for executable kinds — only
    /// the fields the kind's mapping admits, so what validation and
    /// apply see is exactly what the app can execute. Empty object
    /// for deletes and advice.
    pub change: serde_json::Value,
    /// The full text for advice items (the model's `text`, or the
    /// preserved reasoning of an item demoted to advice).
    pub advice_text: Option<String>,
    /// The model's rationale (prose; rendered in the row expandable).
    pub rationale: String,
    /// The model's own expected-effect prose (expandable only; the
    /// row's expected-effect line is app-computed or absent).
    pub expected_effect: String,
    pub confidence: Confidence,
    pub decision: Decision,
    /// This item's fingerprint was denied in a previous plan; it
    /// arrived pre-denied (design §6). A convenience marker — the
    /// operator can always flip it.
    #[serde(default)]
    pub previously_denied: bool,
    /// Apply outcome once an apply run has covered this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_outcome: Option<ApplyOutcome>,
    /// Failure reason when `apply_outcome` is `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_error: Option<String>,
}

/// One consultation's plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// The model's one-line summary (rendered only in an expandable).
    pub summary: String,
    /// The operator directive that scoped this consultation
    /// (addendum A.2): stored verbatim on the plan it produced,
    /// shown on the plan header, and offered as the next
    /// consultation's default directive. Empty when the
    /// consultation had none. Serde-defaulted so plan files
    /// written before Phase 5 stay readable.
    #[serde(default)]
    pub directive: String,
    pub items: Vec<PlanItem>,
    /// Experiment proposals (addendum A.3): a sibling array,
    /// NOT plan items — experiments are ephemeral, measured,
    /// and self-cleaning, while items mutate configuration.
    /// Serde-defaulted so plan files written before Phase 5e
    /// stay readable.
    #[serde(default)]
    pub experiments: Vec<crate::experiment::ExperimentProposal>,
    /// Experiment entries dropped at parse for schema failures
    /// (including an empty `serves` — a proposal serving
    /// neither the directive nor a purpose need is advice, not
    /// an experiment; the parser drops it and counts it here).
    #[serde(default)]
    pub experiments_dropped_invalid: usize,
    /// Experiment entries cut by the 3-proposal cap (§A.3).
    #[serde(default)]
    pub experiments_truncated: usize,
    /// Id of the backend that was consulted.
    pub consulted_backend: String,
    /// Consultation completion time (RFC 3339).
    pub created_at: String,
    /// Hash of the configuration the plan was generated against;
    /// compared at apply time to detect drift (design §5.1).
    pub config_hash: String,
    /// Items dropped at parse for schema failures (design §4).
    pub dropped_invalid: usize,
    /// Items cut by the 25-item cap (design §4).
    pub truncated: usize,
    /// Raw fragments of dropped items, shown with the raw response.
    pub dropped_fragments: Vec<String>,
    /// The model's full raw response, kept with the plan so a parse
    /// failure or an odd item can be inspected after the fact.
    pub raw_response: String,
    /// The exact context document that was sent (design §1.2: the
    /// operator can read everything that left the page).
    pub context: serde_json::Value,
}

/// Canonical JSON for fingerprinting: object keys sorted at every
/// depth, so the same change hashes identically regardless of the
/// key order the model emitted.
pub fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// An item's fingerprint: hash of kind + target + change (design §4).
pub fn fingerprint(kind: PlanKind, target: Option<&str>, change: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_str().as_bytes());
    h.update(b"\n");
    h.update(target.unwrap_or("").as_bytes());
    h.update(b"\n");
    h.update(canonical_json(change).as_bytes());
    format!("{:x}", h.finalize())
}

/// Hash of a whole configuration (serialized), for the plan's
/// staleness check. The caller serializes the config; this hashes
/// whatever bytes represent "the configuration" consistently.
pub fn config_fingerprint(serialized: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(serialized);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_is_key_order_insensitive() {
        let a = json!({"n_ctx": 65536, "n_batch": 512});
        let b = json!({"n_batch": 512, "n_ctx": 65536});
        assert_eq!(
            fingerprint(PlanKind::BackendSetFlags, Some("coder"), &a),
            fingerprint(PlanKind::BackendSetFlags, Some("coder"), &b)
        );
        let c = json!({"n_ctx": 32768, "n_batch": 512});
        assert_ne!(
            fingerprint(PlanKind::BackendSetFlags, Some("coder"), &a),
            fingerprint(PlanKind::BackendSetFlags, Some("coder"), &c)
        );
    }

    #[test]
    fn plan_without_directive_loads_with_empty_directive() {
        // Back-compat (addendum A.2): a plan file written before
        // Phase 5 has no `directive` key; it must still load, with
        // the directive empty.
        let old = json!({
            "summary": "s",
            "items": [],
            "consulted_backend": "planner",
            "created_at": "2026-10-17T00:00:00+00:00",
            "config_hash": "hash",
            "dropped_invalid": 0,
            "truncated": 0,
            "dropped_fragments": [],
            "raw_response": "{}",
            "context": {},
        });
        let plan: Plan = serde_json::from_value(old).unwrap();
        assert_eq!(plan.directive, "");
        assert!(plan.experiments.is_empty());
        assert_eq!(plan.consulted_backend, "planner");
        // And a plan carrying a directive round-trips verbatim.
        let mut with = plan.clone();
        with.directive = "reduce coder latency".to_string();
        let text = serde_json::to_string(&with).unwrap();
        assert!(text.contains("\"directive\":\"reduce coder latency\""));
        let back: Plan = serde_json::from_str(&text).unwrap();
        assert_eq!(back.directive, "reduce coder latency");
    }

    #[test]
    fn kind_round_trips() {
        for k in [
            PlanKind::BackendCreate,
            PlanKind::BackendDelete,
            PlanKind::BackendSetContext,
            PlanKind::BackendSetFlags,
            PlanKind::BackendSetOffload,
            PlanKind::BackendSetPlacement,
            PlanKind::BackendSetModel,
            PlanKind::BackendSetEnabled,
            PlanKind::GroupSetMembers,
            PlanKind::GroupSetStrategy,
            PlanKind::GroupSetFallbacks,
            PlanKind::GroupCreate,
            PlanKind::GroupDelete,
            PlanKind::ModelAdd,
            PlanKind::Advice,
        ] {
            assert_eq!(PlanKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(PlanKind::parse("gateway.set_port"), None);
    }
}
