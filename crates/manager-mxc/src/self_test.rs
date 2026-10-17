//! Policy self-test (spec §4.1 wizard step 9, §14 `mxc_run_self_test`).
//!
//! A fixed set of benign and malicious probes evaluated against a policy
//! with [`decide`](crate::decide::decide). Nothing is executed and no
//! network traffic is sent: probes are pure policy evaluations, so the
//! self-test is harmless by construction.
//!
//! Each probe carries the decision a sane default-deny policy should make;
//! `passed` is `expected == actual` against the policy under test. Run
//! against [`Policy::default_policy`] every case passes; run against a
//! custom policy, failures show exactly where it drifted from the baseline
//! (e.g. a loosened `.ssh` rule or an extra egress allow).

use serde::{Deserialize, Serialize};

use crate::decide::{decide_with_env, AccessKind, AccessRequest};
use crate::{Policy, PolicyDecision};

/// Whether a probe models legitimate tool behavior or an attack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeKind {
    Benign,
    Malicious,
}

/// One self-test probe: expected vs actual decision against the policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfTestCase {
    pub name: String,
    pub description: String,
    pub kind: ProbeKind,
    pub request: AccessRequest,
    /// The decision a sane default-deny policy should make.
    pub expected: PolicyDecision,
    pub actual: PolicyDecision,
    pub passed: bool,
}

/// Pass/fail summary of a self-test run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfTestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

/// Summarize self-test cases.
pub fn summarize_self_test(cases: &[SelfTestCase]) -> SelfTestSummary {
    let passed = cases.iter().filter(|c| c.passed).count();
    SelfTestSummary {
        total: cases.len(),
        passed,
        failed: cases.len() - passed,
    }
}

struct Probe {
    name: &'static str,
    description: &'static str,
    kind: ProbeKind,
    request: AccessRequest,
    expected: PolicyDecision,
}

/// The fixed probe set: benign tool behavior first, then malicious probes.
fn probes() -> Vec<Probe> {
    let file = |kind: AccessKind, path: &str| AccessRequest {
        kind,
        path_or_host: path.to_string(),
        port: None,
    };
    let net = |host: &str, port: u16| AccessRequest {
        kind: AccessKind::Network,
        path_or_host: host.to_string(),
        port: Some(port),
    };
    vec![
        Probe {
            name: "read-project-file",
            description: "Read a file under the project root",
            kind: ProbeKind::Benign,
            request: file(
                AccessKind::FileRead,
                "%USERPROFILE%\\Projects\\demo\\src\\main.rs",
            ),
            expected: PolicyDecision::Allow,
        },
        Probe {
            name: "write-project-file",
            description: "Write a file under the project root",
            kind: ProbeKind::Benign,
            request: file(
                AccessKind::FileWrite,
                "%USERPROFILE%\\Projects\\demo\\src\\main.rs",
            ),
            expected: PolicyDecision::Allow,
        },
        Probe {
            name: "read-node-runtime",
            description: "Read the Node.js runtime (read-only tool runtime)",
            kind: ProbeKind::Benign,
            request: file(AccessKind::FileRead, "%PROGRAMFILES%\\nodejs\\node.exe"),
            expected: PolicyDecision::Allow,
        },
        Probe {
            name: "loopback-gateway",
            description: "Reach the LiteLLM gateway on loopback :4000",
            kind: ProbeKind::Benign,
            request: net("127.0.0.1", 4000),
            expected: PolicyDecision::Allow,
        },
        Probe {
            name: "loopback-backend",
            description: "Reach a llama-server backend on loopback :8081",
            kind: ProbeKind::Benign,
            request: net("127.0.0.1", 8081),
            expected: PolicyDecision::Allow,
        },
        Probe {
            name: "github-api",
            description: "Reach the GitHub API (MCP server + update checks)",
            kind: ProbeKind::Benign,
            request: net("api.github.com", 443),
            expected: PolicyDecision::Allow,
        },
        Probe {
            name: "read-ssh-key",
            description: "Read an SSH private key",
            kind: ProbeKind::Malicious,
            request: file(AccessKind::FileRead, "%USERPROFILE%\\.ssh\\id_ed25519"),
            expected: PolicyDecision::Deny,
        },
        Probe {
            name: "write-ssh-config",
            description: "Modify the SSH client config",
            kind: ProbeKind::Malicious,
            request: file(AccessKind::FileWrite, "%USERPROFILE%\\.ssh\\config"),
            expected: PolicyDecision::Deny,
        },
        Probe {
            name: "read-credential-store",
            description: "Read the Windows credential store",
            kind: ProbeKind::Malicious,
            request: file(
                AccessKind::FileRead,
                "%APPDATA%\\Microsoft\\Credentials\\blob",
            ),
            expected: PolicyDecision::Deny,
        },
        Probe {
            name: "read-documents",
            description: "Read an unrelated file under Documents",
            kind: ProbeKind::Malicious,
            request: file(
                AccessKind::FileRead,
                "%USERPROFILE%\\Documents\\budget.xlsx",
            ),
            expected: PolicyDecision::Deny,
        },
        Probe {
            name: "non-loopback-egress",
            description: "HTTPS egress to a non-allowlisted external host",
            kind: ProbeKind::Malicious,
            request: net("203.0.113.25", 443),
            expected: PolicyDecision::Deny,
        },
        Probe {
            name: "unknown-host-http",
            description: "Plain HTTP to an unknown host",
            kind: ProbeKind::Malicious,
            request: net("untrusted.example.com", 80),
            expected: PolicyDecision::Deny,
        },
    ]
}

/// Run the self-test against `policy` using the process environment for
/// `%VAR%` expansion in rule paths.
pub fn self_test(policy: &Policy) -> Vec<SelfTestCase> {
    self_test_with_env(policy, &|name| std::env::var(name).ok())
}

/// [`self_test`] with an explicit `%VAR%` resolver (for tests/installers).
pub fn self_test_with_env(
    policy: &Policy,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<SelfTestCase> {
    probes()
        .into_iter()
        .map(|p| {
            let actual = decide_with_env(policy, &p.request, lookup);
            SelfTestCase {
                name: p.name.to_string(),
                description: p.description.to_string(),
                kind: p.kind,
                request: p.request,
                expected: p.expected,
                actual,
                passed: actual == p.expected,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    /// Fixed `%VAR%` values so tests are hermetic on any OS.
    fn test_lookup(name: &str) -> Option<String> {
        match name {
            "USERPROFILE" => Some("C:\\Users\\tester".to_string()),
            "PROGRAMFILES" => Some("C:\\Program Files".to_string()),
            "LOCALAPPDATA" => Some("C:\\Users\\tester\\AppData\\Local".to_string()),
            "APPDATA" => Some("C:\\Users\\tester\\AppData\\Roaming".to_string()),
            _ => None,
        }
    }

    #[test]
    fn default_policy_passes_every_probe() {
        let cases = self_test_with_env(&Policy::default_policy(), &test_lookup);
        let summary = summarize_self_test(&cases);
        assert_eq!(summary.total, 12);
        assert_eq!(
            summary.failed,
            0,
            "failures: {:?}",
            cases
                .iter()
                .filter(|c| !c.passed)
                .map(|c| &c.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(summary.passed, summary.total);
    }

    #[test]
    fn deny_all_policy_fails_only_benign_probes() {
        let cases = self_test_with_env(&crate::policy::empty_deny_all(), &test_lookup);
        let summary = summarize_self_test(&cases);
        let benign_failed: Vec<_> = cases
            .iter()
            .filter(|c| c.kind == ProbeKind::Benign && !c.passed)
            .collect();
        let malicious_failed: Vec<_> = cases
            .iter()
            .filter(|c| c.kind == ProbeKind::Malicious && !c.passed)
            .collect();
        assert_eq!(benign_failed.len(), 6, "deny-all blocks every benign probe");
        assert!(
            malicious_failed.is_empty(),
            "deny-all must still deny every malicious probe"
        );
        assert_eq!(summary.failed, 6);
    }

    #[test]
    fn loosened_policy_shows_up_as_probe_failure() {
        let mut policy = Policy::default_policy();
        // Simulate a user loosening the .ssh deny into a read grant (Deny
        // always wins over same-specificity grants, so replace the rule).
        policy
            .filesystem
            .retain(|r| r.path != "%USERPROFILE%\\.ssh");
        policy.filesystem.push(crate::FsRule {
            path: "%USERPROFILE%\\.ssh".to_string(),
            access: crate::FsAccess::Read,
        });
        let cases = self_test_with_env(&policy, &test_lookup);
        let ssh = cases
            .iter()
            .find(|c| c.name == "read-ssh-key")
            .expect("ssh probe");
        assert!(!ssh.passed, "loosened .ssh rule must fail the probe");
        assert_eq!(ssh.actual, PolicyDecision::Allow);
        assert_eq!(ssh.expected, PolicyDecision::Deny);
    }

    #[test]
    fn probe_names_are_unique() {
        let cases = self_test_with_env(&Policy::default_policy(), &test_lookup);
        let mut names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), cases.len());
    }

    #[test]
    fn cases_serialize_for_ui() {
        let cases = self_test_with_env(&Policy::default_policy(), &test_lookup);
        let json = serde_json::to_value(&cases).unwrap();
        assert!(json.is_array());
        assert_eq!(json[0]["kind"], "benign");
        assert_eq!(json[0]["expected"], "allow");
        assert!(json[0]["passed"].as_bool().unwrap());
        let summary = serde_json::to_value(summarize_self_test(&cases)).unwrap();
        assert_eq!(summary["total"], 12);
    }
}
