//! Fail-closed validation of a policy document.
//!
//! Errors (→ `valid == false`):
//! - `default_deny` not set
//! - filesystem paths that are not absolute (or deferred `%KNOWN_VAR%\`)
//! - empty paths/hosts, duplicate credential names
//! - ports out of range (0 is invalid; `u16` caps the top)
//! - allow-all network rules (`*` with Allow/Prompt)
//! - any value that looks like an embedded secret — **the policy JSON must
//!   never carry secret values**; error messages redact the value.
//!
//! Warnings (valid stays true): missing explicit deny rules for `.ssh` /
//! credential stores, missing loopback allowlist ports, duplicate rules,
//! completely empty policies (legitimate Learning-mode start).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::decide::loopback_allowed;
use crate::policy::is_absolute_or_deferred;
use crate::{FsAccess, MxcError, Policy, PolicyDecision};

/// Validation report for a policy document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Required loopback ports: LiteLLM :4000 and backends :8081–:8084.
pub const REQUIRED_LOOPBACK_PORTS: &[u16] = &[4000, 8081, 8082, 8083, 8084];

/// Heuristic: does this value look like an embedded secret (API key, token,
/// password, private key) rather than a path/host/reference name?
///
/// This is intentionally conservative on the *prefix* side and strict on the
/// *key=value* side. It is a safety net, not a substitute for never putting
/// secrets in the policy in the first place.
pub fn looks_like_secret(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    let lower = v.to_lowercase();

    // Known token prefixes (sk- covers OpenAI/OpenRouter-style keys).
    const PREFIXES: &[&str] = &[
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "glpat-",
        "dop_v1_",
        "hf_",
        "AIza",
    ];
    for p in PREFIXES {
        if lower.starts_with(p) && v.len() > p.len() + 4 {
            return true;
        }
    }
    // AWS access key id: AKIA + 16 alphanumerics.
    if v.len() == 20 && v.starts_with("AKIA") && v[4..].chars().all(|c| c.is_ascii_alphanumeric()) {
        return true;
    }
    // PEM private key block.
    if v.contains("-----BEGIN") && lower.contains("private key") {
        return true;
    }
    // Bearer token.
    if lower.starts_with("bearer ") && v.len() > 12 {
        return true;
    }
    // key=value / key:value with a substantial value. The separator guards
    // against false positives like `C:\tokens\file.txt`.
    const KEYS: &[&str] = &[
        "password",
        "passwd",
        "pwd",
        "secret",
        "token",
        "api_key",
        "apikey",
        "access_key",
        "private_key",
        "client_secret",
    ];
    for key in KEYS {
        for sep in ['=', ':'] {
            let needle = format!("{key}{sep}");
            if let Some(idx) = lower.find(&needle) {
                let val = v[idx + needle.len()..]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'');
                // A bare `token:` label with nothing after it is not a secret.
                if val.len() >= 8 {
                    return true;
                }
            }
        }
    }
    false
}

/// Redact a value for error messages: show a short prefix so the operator can
/// locate the offending field, never the value itself.
fn redact(value: &str) -> String {
    let prefix: String = value.chars().take(6).collect();
    format!("{prefix}…<redacted>")
}

fn check_secret(field: &str, value: &str, errors: &mut Vec<String>) {
    if looks_like_secret(value) {
        errors.push(format!(
            "{field} looks like an embedded secret ({}) — secrets must live in Windows Credential Manager, referenced by target name only",
            redact(value)
        ));
    }
}

/// Validate a policy document. Fails closed: any error → `valid == false`.
pub fn validate(policy: &Policy) -> Result<ValidationReport, MxcError> {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if !policy.default_deny {
        errors.push("default_deny is false: the policy must be default-deny".to_string());
    }

    for (i, rule) in policy.filesystem.iter().enumerate() {
        let field = format!("filesystem[{i}].path");
        if rule.path.trim().is_empty() {
            errors.push(format!("filesystem[{i}]: empty path"));
            continue;
        }
        if !is_absolute_or_deferred(&rule.path) {
            errors.push(format!(
                "filesystem[{i}]: path is not absolute: {}",
                redact(&rule.path)
            ));
        }
        check_secret(&field, &rule.path, &mut errors);
    }

    for (i, rule) in policy.network.iter().enumerate() {
        let host = rule.host.trim();
        if host.is_empty() {
            errors.push(format!("network[{i}]: empty host"));
        } else {
            if (host == "*" || host == "*.*")
                && matches!(
                    rule.decision,
                    PolicyDecision::Allow | PolicyDecision::Prompt
                )
            {
                errors.push(format!(
                    "network[{i}]: allow-all rule for host '*': default-deny prohibits this"
                ));
            }
            check_secret(&format!("network[{i}].host"), host, &mut errors);
        }
        for port in &rule.ports {
            if *port == 0 {
                errors.push(format!("network[{i}]: port 0 is invalid"));
            }
        }
    }

    let mut seen_names: HashSet<String> = HashSet::new();
    for (i, cred) in policy.credentials.iter().enumerate() {
        if cred.name.trim().is_empty() {
            errors.push(format!("credentials[{i}]: empty name"));
        }
        if cred.target.trim().is_empty() {
            errors.push(format!("credentials[{i}]: empty target"));
        }
        if !seen_names.insert(cred.name.to_lowercase()) {
            errors.push(format!(
                "credentials[{i}]: duplicate credential name {}",
                redact(&cred.name)
            ));
        }
        check_secret(&format!("credentials[{i}].name"), &cred.name, &mut errors);
        check_secret(
            &format!("credentials[{i}].target"),
            &cred.target,
            &mut errors,
        );
    }

    // Warnings below: the policy is still usable, but review is warranted.

    let has_deny_for = |needle: &str| {
        policy
            .filesystem
            .iter()
            .any(|r| r.access == FsAccess::Deny && r.path.to_lowercase().contains(needle))
    };
    if !has_deny_for(".ssh") {
        warnings.push("no explicit deny rule covering .ssh directories".to_string());
    }
    if !has_deny_for("credential") {
        warnings.push("no explicit deny rule covering credential stores".to_string());
    }

    let missing: Vec<u16> = REQUIRED_LOOPBACK_PORTS
        .iter()
        .copied()
        .filter(|p| !loopback_allowed(policy, *p))
        .collect();
    if !missing.is_empty() {
        warnings.push(format!(
            "loopback not allowed for required ports: {missing:?} (LiteLLM :4000, backends :8081–:8084)"
        ));
    }

    // Exact-duplicate rules are dead weight; flag them.
    let mut seen_rules: HashSet<String> = HashSet::new();
    for (i, rule) in policy.filesystem.iter().enumerate() {
        let key = format!("fs:{}:{:?}", rule.path.to_lowercase(), rule.access);
        if !seen_rules.insert(key) {
            warnings.push(format!("filesystem[{i}]: duplicate rule"));
        }
    }

    if policy.filesystem.is_empty() && policy.network.is_empty() {
        warnings.push(
            "policy has no rules at all: everything is denied (fine for a Learning-mode start)"
                .to_string(),
        );
    }

    Ok(ValidationReport {
        valid: errors.is_empty(),
        errors,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{empty_deny_all, CredentialRef, FsRule, NetRule, Policy};

    #[test]
    fn default_policy_validates_clean() {
        let report = validate(&Policy::default_policy()).expect("validate");
        assert!(report.valid, "errors: {:?}", report.errors);
        assert!(report.errors.is_empty());
        assert!(
            report.warnings.is_empty(),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn secret_in_credential_target_rejected_and_redacted() {
        let mut p = Policy::default_policy();
        p.credentials.push(CredentialRef {
            name: "openrouter".to_string(),
            target: "sk-or-v1-abc123def456ghi789".to_string(),
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("embedded secret")));
        // the full secret must not appear in any error message
        for e in &report.errors {
            assert!(
                !e.contains("sk-or-v1-abc123def456ghi789"),
                "secret leaked into error: {e}"
            );
        }
    }

    #[test]
    fn secret_key_value_in_host_rejected() {
        let mut p = Policy::default_policy();
        p.network.push(NetRule {
            host: "api.example.com?token=abcdef1234567890".to_string(),
            ports: vec![443],
            decision: PolicyDecision::Allow,
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("embedded secret")));
    }

    #[test]
    fn github_pat_prefix_rejected() {
        let mut p = Policy::default_policy();
        p.credentials.push(CredentialRef {
            name: "github".to_string(),
            target: "ghp_abcdefghij1234567890".to_string(),
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
    }

    #[test]
    fn benign_values_not_flagged() {
        // Paths/names containing the *word* "token" without a value are fine.
        assert!(!looks_like_secret("C:\\Tools\\tokens\\readme.txt"));
        assert!(!looks_like_secret("local-llm-service-manager/github"));
        assert!(!looks_like_secret("api.github.com"));
        assert!(!looks_like_secret("github"));
        assert!(!looks_like_secret(""));
        // ...but actual values are caught
        assert!(looks_like_secret("sk-abc123xyz789"));
        assert!(looks_like_secret("password=hunter2hunter2"));
        assert!(looks_like_secret("api_key: deadbeefcafebabe"));
        assert!(looks_like_secret("Bearer eyJhbGciOiJIUzI1NiJ9"));
        assert!(looks_like_secret(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE..."
        ));
        assert!(looks_like_secret("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn non_absolute_path_rejected() {
        let mut p = Policy::default_policy();
        p.filesystem.push(FsRule {
            path: "Projects\\app".to_string(),
            access: FsAccess::ReadWrite,
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("not absolute")));
    }

    #[test]
    fn port_zero_rejected() {
        let mut p = Policy::default_policy();
        p.network.push(NetRule {
            host: "example.com".to_string(),
            ports: vec![0],
            decision: PolicyDecision::Allow,
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("port 0")));
    }

    #[test]
    fn allow_all_rejected() {
        let mut p = Policy::default_policy();
        p.network.push(NetRule {
            host: "*".to_string(),
            ports: vec![443],
            decision: PolicyDecision::Allow,
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("allow-all")));
    }

    #[test]
    fn default_deny_false_rejected() {
        let mut p = Policy::default_policy();
        p.default_deny = false;
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("default_deny")));
    }

    #[test]
    fn duplicate_credential_names_rejected() {
        let mut p = Policy::default_policy();
        p.credentials.push(CredentialRef {
            name: "GitHub".to_string(), // case-insensitive duplicate
            target: "local-llm-service-manager/github-2".to_string(),
        });
        let report = validate(&p).expect("validate");
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("duplicate")));
    }

    #[test]
    fn empty_policy_warns_but_valid() {
        let report = validate(&empty_deny_all()).expect("validate");
        assert!(report.valid);
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn missing_loopback_warns() {
        let mut p = Policy::default_policy();
        p.network.retain(|r| r.host != "127.0.0.1");
        let report = validate(&p).expect("validate");
        assert!(report.valid);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("loopback not allowed")));
    }

    #[test]
    fn serialized_policy_carries_no_secrets() {
        let json = serde_json::to_string(&Policy::default_policy()).expect("serialize");
        // every string field in the policy must pass the secret scan
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        fn walk(val: &serde_json::Value) {
            match val {
                serde_json::Value::String(s) => {
                    assert!(
                        !looks_like_secret(s),
                        "secret-looking value in policy JSON: {s}"
                    )
                }
                serde_json::Value::Array(a) => a.iter().for_each(walk),
                serde_json::Value::Object(o) => o.values().for_each(walk),
                _ => {}
            }
        }
        walk(&v);
    }
}
