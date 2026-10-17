//! The sandbox policy document model.
//!
//! Rule paths may use a leading `%VAR%\` prefix where `VAR` is one of
//! [`KNOWN_ENV_VARS`]; the installer expands these at setup time. Everything
//! else must be an absolute Windows path (`C:\…` or `\\server\share`).

use serde::{Deserialize, Serialize};

use crate::PolicyDecision;

/// Environment variables allowed as a leading `%VAR%\` prefix in rule paths.
/// Deferred absolute paths: expanded by the installer at setup time.
pub const KNOWN_ENV_VARS: &[&str] = &[
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "SYSTEMDRIVE",
    "WINDIR",
    "TEMP",
    "TMP",
];

/// Filesystem access level for a path rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsAccess {
    Read,
    Write,
    ReadWrite,
    Deny,
}

impl FsAccess {
    /// Does this level grant file reads?
    pub fn grants_read(self) -> bool {
        matches!(self, FsAccess::Read | FsAccess::ReadWrite)
    }

    /// Does this level grant file writes?
    pub fn grants_write(self) -> bool {
        matches!(self, FsAccess::Write | FsAccess::ReadWrite)
    }
}

/// One filesystem rule: approved project dirs read/write, tool runtimes
/// read-only, `.ssh`/credential stores/personal data denied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsRule {
    pub path: String,
    pub access: FsAccess,
}

/// One network rule. Loopback allowances cover LiteLLM :4000 and the
/// model-server ports; named endpoints cover GitHub, Google APIs, OpenRouter.
///
/// An empty `ports` list matches no ports for `Allow`/`Prompt` (fail closed);
/// for `Deny` it matches all ports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetRule {
    pub host: String,
    pub ports: Vec<u16>,
    pub decision: PolicyDecision,
}

/// A credential reference: Credential Manager target name only.
/// The value is resolved per invocation, never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRef {
    pub name: String,
    pub target: String,
}

/// The sandbox policy document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version: u32,
    pub filesystem: Vec<FsRule>,
    pub network: Vec<NetRule>,
    pub credentials: Vec<CredentialRef>,
    /// Default-deny: anything not explicitly allowed is denied.
    pub default_deny: bool,
    /// Container UI access. Spec §11.2: disabled unless a tool explicitly needs it.
    #[serde(default)]
    pub ui_access: bool,
}

/// Project directories the default policy grants read/write to.
/// The installer replaces/extends these with the user's actual project roots.
pub fn default_project_dirs() -> Vec<String> {
    vec!["%USERPROFILE%\\Projects".to_string()]
}

/// Pure deny-all starting point for Learning-mode observation.
/// Prefer [`Policy::default_policy`] for the recommended baseline.
pub fn empty_deny_all() -> Policy {
    Policy {
        version: 1,
        filesystem: Vec::new(),
        network: Vec::new(),
        credentials: Vec::new(),
        default_deny: true,
        ui_access: false,
    }
}

impl Policy {
    /// The spec's proposed baseline policy (spec §11.2):
    /// read/write to project directories, read-only tool runtimes,
    /// deny `.ssh`/credential stores/unrelated Documents, default-deny
    /// network with loopback allowlist (:4000, :8081–:8084) plus the GitHub,
    /// Google API, and OpenRouter endpoints, credentials by reference only,
    /// UI access off.
    pub fn default_policy() -> Self {
        let mut filesystem: Vec<FsRule> = default_project_dirs()
            .into_iter()
            .map(|path| FsRule {
                path,
                access: FsAccess::ReadWrite,
            })
            .collect();
        // Required tool runtimes: read-only.
        filesystem.extend([
            FsRule {
                path: "%PROGRAMFILES%\\nodejs".to_string(),
                access: FsAccess::Read,
            },
            FsRule {
                path: "%PROGRAMFILES%\\Git".to_string(),
                access: FsAccess::Read,
            },
            FsRule {
                path: "%LOCALAPPDATA%\\Programs\\Python".to_string(),
                access: FsAccess::Read,
            },
            // Sensitive locations: denied outright.
            FsRule {
                path: "%USERPROFILE%\\.ssh".to_string(),
                access: FsAccess::Deny,
            },
            FsRule {
                path: "%APPDATA%\\Microsoft\\Credentials".to_string(),
                access: FsAccess::Deny,
            },
            FsRule {
                path: "%LOCALAPPDATA%\\Microsoft\\Credentials".to_string(),
                access: FsAccess::Deny,
            },
            FsRule {
                path: "%USERPROFILE%\\Documents".to_string(),
                access: FsAccess::Deny,
            },
        ]);

        Self {
            version: 1,
            filesystem,
            network: vec![
                NetRule {
                    host: "127.0.0.1".to_string(),
                    ports: vec![4000, 8081, 8082, 8083, 8084],
                    decision: PolicyDecision::Allow,
                },
                NetRule {
                    host: "api.github.com".to_string(),
                    ports: vec![443],
                    decision: PolicyDecision::Allow,
                },
                NetRule {
                    host: "github.com".to_string(),
                    ports: vec![443],
                    decision: PolicyDecision::Allow,
                },
                NetRule {
                    host: "www.googleapis.com".to_string(),
                    ports: vec![443],
                    decision: PolicyDecision::Allow,
                },
                NetRule {
                    host: "drive.googleapis.com".to_string(),
                    ports: vec![443],
                    decision: PolicyDecision::Allow,
                },
                NetRule {
                    host: "oauth2.googleapis.com".to_string(),
                    ports: vec![443],
                    decision: PolicyDecision::Allow,
                },
                NetRule {
                    host: "openrouter.ai".to_string(),
                    ports: vec![443],
                    decision: PolicyDecision::Allow,
                },
            ],
            credentials: vec![
                CredentialRef {
                    name: "github".to_string(),
                    target: "local-llm-service-manager/github".to_string(),
                },
                CredentialRef {
                    name: "google".to_string(),
                    target: "local-llm-service-manager/google".to_string(),
                },
                CredentialRef {
                    name: "openrouter".to_string(),
                    target: "local-llm-service-manager/openrouter".to_string(),
                },
            ],
            default_deny: true,
            ui_access: false,
        }
    }
}

/// Expand `%VAR%` occurrences using `lookup`. Unknown variables are left
/// as-is (the rule then simply won't match — fail closed).
pub fn expand_env_vars(path: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        match rest[start + 1..].find('%') {
            Some(end) => {
                let name = &rest[start + 1..start + 1 + end];
                out.push_str(&rest[..start]);
                match lookup(name) {
                    Some(val) => out.push_str(&val),
                    None => out.push_str(&rest[start..start + 1 + end + 1]),
                }
                rest = &rest[start + 1 + end + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Normalize a Windows path for prefix matching: `/` → `\`, drop trailing
/// separators (except a drive root like `C:\`), lowercase.
pub(crate) fn normalize_path(path: &str) -> String {
    let mut p = path.replace('/', "\\");
    while p.len() > 3 && p.ends_with('\\') {
        p.pop();
    }
    // A bare drive like `C:` is not a usable prefix root; keep it as-is.
    p.to_lowercase()
}

/// True for `C:\…`, `C:…`, `\\server\share…`, or a leading `%KNOWN_VAR%\`
/// deferred-absolute path.
pub(crate) fn is_absolute_or_deferred(path: &str) -> bool {
    let p = path.replace('/', "\\");
    if let Some(rest) = p.strip_prefix('%') {
        if let Some(end) = rest.find('%') {
            let var = rest[..end].to_ascii_uppercase();
            let after = &rest[end + 1..];
            return KNOWN_ENV_VARS.contains(&var.as_str())
                && (after.is_empty() || after.starts_with('\\'));
        }
        return false;
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return true;
    }
    if p.starts_with("\\\\") && p.len() > 2 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn expand_env_vars_basic() {
        let l = lookup(&[("USERPROFILE", "C:\\Users\\Pete")]);
        assert_eq!(
            expand_env_vars("%USERPROFILE%\\Projects", &l),
            "C:\\Users\\Pete\\Projects"
        );
        // unknown variable is left as-is (fail closed downstream)
        assert_eq!(expand_env_vars("%NOPE%\\x", &l), "%NOPE%\\x");
        // unterminated % is left as-is
        assert_eq!(expand_env_vars("C:\\100%\\x", &l), "C:\\100%\\x");
    }

    #[test]
    fn normalize_path_cases() {
        assert_eq!(normalize_path("C:/Users/Pete/"), "c:\\users\\pete");
        assert_eq!(normalize_path("C:\\"), "c:\\");
        assert_eq!(
            normalize_path("c:\\USERS\\PETE\\Projects"),
            "c:\\users\\pete\\projects"
        );
    }

    #[test]
    fn absolute_detection() {
        assert!(is_absolute_or_deferred("C:\\Users\\Pete"));
        assert!(is_absolute_or_deferred("d:/data"));
        assert!(is_absolute_or_deferred("\\\\server\\share\\x"));
        assert!(is_absolute_or_deferred("%USERPROFILE%\\Projects"));
        assert!(is_absolute_or_deferred("%APPDATA%"));
        assert!(!is_absolute_or_deferred("Projects\\app"));
        assert!(!is_absolute_or_deferred("%NOPE%\\x"));
        assert!(!is_absolute_or_deferred("relative/path"));
    }

    #[test]
    fn default_policy_shape() {
        let p = Policy::default_policy();
        assert_eq!(p.version, 1);
        assert!(p.default_deny);
        assert!(!p.ui_access);
        assert_eq!(p.credentials.len(), 3);
        // loopback rule covers the gateway + all four backends
        let lo = p
            .network
            .iter()
            .find(|r| r.host == "127.0.0.1")
            .expect("loopback rule");
        assert_eq!(lo.ports, vec![4000, 8081, 8082, 8083, 8084]);
        assert_eq!(lo.decision, PolicyDecision::Allow);
    }

    #[test]
    fn empty_deny_all_is_empty() {
        let p = empty_deny_all();
        assert!(p.default_deny);
        assert!(p.filesystem.is_empty());
        assert!(p.network.is_empty());
        assert!(p.credentials.is_empty());
    }
}
