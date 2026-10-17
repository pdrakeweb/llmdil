//! MXC (Microsoft Execution Containers) sandbox policy (spec §11).
//!
//! Sandboxes agent *tool execution only* — inference and LiteLLM stay on the
//! host; contained agents reach them over loopback per policy. Policies are
//! default-deny. Secrets are never stored in the policy JSON: credentials are
//! resolved from Windows Credential Manager per invocation, referenced here
//! only by target name.
//!
//! Lifecycle: begin in Learning mode, inspect the activity report, then derive
//! an enforced least-privilege policy via [`learning::suggest_rules`].
//!
//! Layout:
//! - [`policy`] — the [`Policy`] document model and [`Policy::default_policy`]
//!   (the spec's proposed baseline).
//! - [`decide`] — the [`decide()`] evaluator for one [`decide::AccessRequest`].
//! - [`validate`] — fail-closed [`validate()`] of a policy document.
//! - [`learning`] — Learning-mode report parsing and rule suggestions.

pub mod decide;
pub mod learning;
pub mod policy;
pub mod self_test;
pub mod validate;

pub use decide::{decide, decide_with_env, AccessKind, AccessRequest};
pub use learning::{
    filter_since, parse_learning_report, suggest_rules, suggest_rules_with_env, LearningEntry,
    LearningReport, ObservationKey, ObservationSummary, RuleSuggestion,
};
pub use policy::{
    default_project_dirs, empty_deny_all, expand_env_vars, CredentialRef, FsAccess, FsRule,
    NetRule, Policy, KNOWN_ENV_VARS,
};
pub use self_test::{
    self_test, self_test_with_env, summarize_self_test, ProbeKind, SelfTestCase, SelfTestSummary,
};
pub use validate::{looks_like_secret, validate, ValidationReport};

use serde::{Deserialize, Serialize};

/// Policy decision for one access request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyDecision {
    Allow,
    Prompt,
    Deny,
}

/// MXC errors.
#[derive(Debug, thiserror::Error)]
pub enum MxcError {
    #[error("policy error: {0}")]
    Policy(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("runtime error: {0}")]
    Runtime(String),
}
