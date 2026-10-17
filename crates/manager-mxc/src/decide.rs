//! The policy decision evaluator: one [`AccessRequest`] → [`PolicyDecision`].
//!
//! Fail-closed throughout: a request that matches no rule is denied when
//! `policy.default_deny` is set; a filesystem rule that does not grant the
//! requested kind denies; an allow/prompt network rule with an empty port list
//! matches nothing.

use serde::{Deserialize, Serialize};

use crate::policy::{expand_env_vars, normalize_path};
use crate::{Policy, PolicyDecision};

/// What kind of access is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    FileRead,
    FileWrite,
    Network,
}

/// One access request to decide on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessRequest {
    pub kind: AccessKind,
    /// Filesystem path for file requests; hostname for network requests.
    pub path_or_host: String,
    /// Destination port for network requests.
    pub port: Option<u16>,
}

/// Decide one request against the policy using the process environment for
/// `%VAR%` expansion in rule paths.
pub fn decide(policy: &Policy, request: &AccessRequest) -> PolicyDecision {
    decide_with_env(policy, request, &|name| std::env::var(name).ok())
}

/// Decide one request against the policy with an explicit `%VAR%` resolver.
/// Useful for tests and for installers that expand variables themselves.
pub fn decide_with_env(
    policy: &Policy,
    request: &AccessRequest,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> PolicyDecision {
    match request.kind {
        AccessKind::FileRead | AccessKind::FileWrite => decide_fs(policy, request, lookup),
        AccessKind::Network => decide_net(policy, request),
    }
}

fn default_decision(policy: &Policy) -> PolicyDecision {
    if policy.default_deny {
        PolicyDecision::Deny
    } else {
        PolicyDecision::Allow
    }
}

fn decide_fs(
    policy: &Policy,
    request: &AccessRequest,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> PolicyDecision {
    let req = normalize_path(&expand_env_vars(&request.path_or_host, lookup));

    // Longest-prefix (most specific) rule wins. Rules at the same specificity
    // union their grants, except Deny always wins — fail closed. This lets the
    // tighten-the-policy flow add a Write rule next to an existing Read rule
    // for the same path and have it take effect.
    let mut best_len: Option<usize> = None;
    let mut denied = false;
    let mut grants_read = false;
    let mut grants_write = false;
    for rule in &policy.filesystem {
        let rule_path = normalize_path(&expand_env_vars(&rule.path, lookup));
        let matches = req == rule_path || req.starts_with(&format!("{rule_path}\\"));
        if !matches {
            continue;
        }
        match best_len {
            Some(bl) if rule_path.len() < bl => continue,
            Some(bl) if rule_path.len() > bl => {
                best_len = Some(rule_path.len());
                denied = false;
                grants_read = false;
                grants_write = false;
            }
            None => {
                best_len = Some(rule_path.len());
            }
            _ => {}
        }
        match rule.access {
            crate::FsAccess::Deny => denied = true,
            access => {
                grants_read |= access.grants_read();
                grants_write |= access.grants_write();
            }
        }
    }

    match best_len {
        None => default_decision(policy),
        Some(_) if denied => PolicyDecision::Deny,
        Some(_) => {
            // Fail closed: a matching rule set that does not grant the
            // requested kind denies rather than falling through.
            let granted = match request.kind {
                AccessKind::FileRead => grants_read,
                AccessKind::FileWrite => grants_write,
                AccessKind::Network => false,
            };
            if granted {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            }
        }
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

/// Port matching for a network rule. Empty port list: matches nothing for
/// Allow/Prompt (fail closed), everything for Deny.
fn ports_match(rule: &crate::NetRule, port: Option<u16>) -> bool {
    if rule.ports.is_empty() {
        return rule.decision == PolicyDecision::Deny;
    }
    match port {
        Some(p) => rule.ports.contains(&p),
        None => false,
    }
}

fn decide_net(policy: &Policy, request: &AccessRequest) -> PolicyDecision {
    let req_host = normalize_host(&request.path_or_host);

    // Pass 1: exact host match. Loopback aliases are equivalent: a rule for
    // any of localhost/127.0.0.1/::1 covers requests to any of them.
    // Pass 2: wildcard suffix match (`*.example.com`).
    // Within a pass, an explicit Deny beats Allow/Prompt (fail closed);
    // otherwise the first matching rule in policy order wins.
    for pass in 0..2 {
        let mut first: Option<PolicyDecision> = None;
        for rule in &policy.network {
            let rule_host = normalize_host(&rule.host);
            let host_match = if pass == 0 {
                if rule_host.starts_with("*.") {
                    continue;
                }
                if is_loopback(&rule_host) || is_loopback(&req_host) {
                    is_loopback(&rule_host) && is_loopback(&req_host)
                } else {
                    rule_host == req_host
                }
            } else if let Some(suffix) = rule_host.strip_prefix("*.") {
                req_host == suffix || req_host.ends_with(&format!(".{suffix}"))
            } else {
                continue;
            };
            if host_match && ports_match(rule, request.port) {
                if rule.decision == PolicyDecision::Deny {
                    return PolicyDecision::Deny;
                }
                if first.is_none() {
                    first = Some(rule.decision);
                }
            }
        }
        if let Some(decision) = first {
            return decision;
        }
    }

    default_decision(policy)
}

/// Is loopback `port` allowed by the policy? Used by validation to check the
/// :4000/:8081–8084 allowlist is present.
pub(crate) fn loopback_allowed(policy: &Policy, port: u16) -> bool {
    decide(
        policy,
        &AccessRequest {
            kind: AccessKind::Network,
            path_or_host: "127.0.0.1".to_string(),
            port: Some(port),
        },
    ) == PolicyDecision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

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

    fn test_lookup() -> impl Fn(&str) -> Option<String> {
        lookup(&[
            ("USERPROFILE", "C:\\Users\\Pete"),
            ("PROGRAMFILES", "C:\\Program Files"),
            ("LOCALAPPDATA", "C:\\Users\\Pete\\AppData\\Local"),
            ("APPDATA", "C:\\Users\\Pete\\AppData\\Roaming"),
        ])
    }

    fn file_req(kind: AccessKind, path: &str) -> AccessRequest {
        AccessRequest {
            kind,
            path_or_host: path.to_string(),
            port: None,
        }
    }

    fn net_req(host: &str, port: u16) -> AccessRequest {
        AccessRequest {
            kind: AccessKind::Network,
            path_or_host: host.to_string(),
            port: Some(port),
        }
    }

    #[test]
    fn project_write_allowed() {
        let p = Policy::default_policy();
        let l = test_lookup();
        let r = decide_with_env(
            &p,
            &file_req(
                AccessKind::FileWrite,
                "C:\\Users\\Pete\\Projects\\app\\main.py",
            ),
            &l,
        );
        assert_eq!(r, PolicyDecision::Allow);
    }

    #[test]
    fn project_read_allowed_forward_slashes() {
        let p = Policy::default_policy();
        let l = test_lookup();
        let r = decide_with_env(
            &p,
            &file_req(AccessKind::FileRead, "C:/Users/Pete/Projects/app/README.md"),
            &l,
        );
        assert_eq!(r, PolicyDecision::Allow);
    }

    #[test]
    fn ssh_read_denied() {
        let p = Policy::default_policy();
        let l = test_lookup();
        let r = decide_with_env(
            &p,
            &file_req(AccessKind::FileRead, "C:\\Users\\Pete\\.ssh\\id_ed25519"),
            &l,
        );
        assert_eq!(r, PolicyDecision::Deny);
    }

    #[test]
    fn documents_denied() {
        let p = Policy::default_policy();
        let l = test_lookup();
        let r = decide_with_env(
            &p,
            &file_req(
                AccessKind::FileRead,
                "C:\\Users\\Pete\\Documents\\taxes\\2025.pdf",
            ),
            &l,
        );
        assert_eq!(r, PolicyDecision::Deny);
    }

    #[test]
    fn runtime_read_allowed_write_denied() {
        let p = Policy::default_policy();
        let l = test_lookup();
        let read = decide_with_env(
            &p,
            &file_req(AccessKind::FileRead, "C:\\Program Files\\nodejs\\node.exe"),
            &l,
        );
        let write = decide_with_env(
            &p,
            &file_req(AccessKind::FileWrite, "C:\\Program Files\\nodejs\\node.exe"),
            &l,
        );
        assert_eq!(read, PolicyDecision::Allow);
        assert_eq!(write, PolicyDecision::Deny);
    }

    #[test]
    fn unmatched_path_denied_by_default() {
        let p = Policy::default_policy();
        let l = test_lookup();
        let r = decide_with_env(
            &p,
            &file_req(
                AccessKind::FileRead,
                "C:\\Windows\\System32\\drivers\\etc\\hosts",
            ),
            &l,
        );
        assert_eq!(r, PolicyDecision::Deny);
    }

    #[test]
    fn longest_prefix_wins() {
        use crate::{FsAccess, FsRule};
        let mut p = Policy::default_policy();
        p.filesystem.push(FsRule {
            path: "C:\\Users\\Pete\\Projects\\secret".to_string(),
            access: FsAccess::Deny,
        });
        let l = test_lookup();
        // inside the denied child → denied despite the allowed parent
        let denied = decide_with_env(
            &p,
            &file_req(
                AccessKind::FileRead,
                "C:\\Users\\Pete\\Projects\\secret\\notes.txt",
            ),
            &l,
        );
        // sibling stays allowed
        let allowed = decide_with_env(
            &p,
            &file_req(
                AccessKind::FileRead,
                "C:\\Users\\Pete\\Projects\\app\\notes.txt",
            ),
            &l,
        );
        assert_eq!(denied, PolicyDecision::Deny);
        assert_eq!(allowed, PolicyDecision::Allow);
    }

    #[test]
    fn loopback_ports_allowed() {
        let p = Policy::default_policy();
        for port in [4000u16, 8081, 8082, 8083, 8084] {
            assert_eq!(
                decide(&p, &net_req("127.0.0.1", port)),
                PolicyDecision::Allow,
                "port {port}"
            );
        }
        // localhost is a loopback alias of the 127.0.0.1 rule
        assert_eq!(
            decide(&p, &net_req("localhost", 8081)),
            PolicyDecision::Allow
        );
        // but a non-allowlisted loopback port is denied
        assert_eq!(
            decide(&p, &net_req("127.0.0.1", 9999)),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn named_endpoints_allowed() {
        let p = Policy::default_policy();
        assert_eq!(
            decide(&p, &net_req("api.github.com", 443)),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&p, &net_req("API.GITHUB.COM", 443)),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&p, &net_req("openrouter.ai", 443)),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&p, &net_req("drive.googleapis.com", 443)),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn external_host_denied() {
        let p = Policy::default_policy();
        assert_eq!(decide(&p, &net_req("evil.com", 443)), PolicyDecision::Deny);
        // allowed host, wrong port → denied
        assert_eq!(
            decide(&p, &net_req("api.github.com", 80)),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn wildcard_host_match() {
        use crate::NetRule;
        let mut p = Policy::default_policy();
        p.network.push(NetRule {
            host: "*.internal.example".to_string(),
            ports: vec![443],
            decision: PolicyDecision::Allow,
        });
        assert_eq!(
            decide(&p, &net_req("svc.internal.example", 443)),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&p, &net_req("other.example", 443)),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn same_path_rules_union_grants() {
        // Tighten-flow scenario: a Write rule added next to an existing Read
        // rule for the same path takes effect (grants union).
        use crate::{FsAccess, FsRule};
        let mut p = crate::policy::empty_deny_all();
        p.filesystem.push(FsRule {
            path: "C:\\Data".to_string(),
            access: FsAccess::Read,
        });
        p.filesystem.push(FsRule {
            path: "C:\\Data".to_string(),
            access: FsAccess::Write,
        });
        let l = test_lookup();
        assert_eq!(
            decide_with_env(&p, &file_req(AccessKind::FileRead, "C:\\Data\\a.txt"), &l),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide_with_env(&p, &file_req(AccessKind::FileWrite, "C:\\Data\\a.txt"), &l),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn deny_beats_allow_at_same_specificity() {
        use crate::{FsAccess, FsRule};
        let mut p = crate::policy::empty_deny_all();
        p.filesystem.push(FsRule {
            path: "C:\\Data".to_string(),
            access: FsAccess::ReadWrite,
        });
        p.filesystem.push(FsRule {
            path: "C:\\Data".to_string(),
            access: FsAccess::Deny,
        });
        let l = test_lookup();
        assert_eq!(
            decide_with_env(&p, &file_req(AccessKind::FileRead, "C:\\Data\\a.txt"), &l),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn prompt_decision_propagates() {
        use crate::NetRule;
        let mut p = Policy::default_policy();
        p.network.push(NetRule {
            host: "review.example.com".to_string(),
            ports: vec![443],
            decision: PolicyDecision::Prompt,
        });
        assert_eq!(
            decide(&p, &net_req("review.example.com", 443)),
            PolicyDecision::Prompt
        );
    }
}
