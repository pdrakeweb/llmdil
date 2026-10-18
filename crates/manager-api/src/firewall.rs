//! Windows Firewall reconciliation for the API listener
//! (PLAN.md §2.4 / decision 2): one inbound rule for the API port,
//! scoped to the local subnet plus the Tailscale range
//! (100.64.0.0/10) — the machine binds all interfaces, the firewall
//! is what keeps the port off the WAN side, and Pete does not
//! port-forward it. The rule is created when the API is enabled and
//! removed when it is disabled; state changes go through the
//! wizard's elevated runner (one UAC prompt per change). All
//! functions are no-ops reporting `Unsupported` off Windows.

use manager_wizard::PlannedCommand;
#[cfg(windows)]
use manager_wizard::SystemOps;

/// The rule's firewall display name (the port is in the name so a
/// port change leaves no ambiguity about which rule is ours).
pub fn rule_name(port: u16) -> String {
    format!("LLMdil Admin API ({port})")
}

/// Remote scopes for the rule: `localsubnet` plus the Tailscale
/// CGNAT range.
pub const REMOTE_SCOPES: &str = "localsubnet,100.64.0.0/10";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirewallStatus {
    /// Rule present with the expected scope and port.
    Ok,
    /// No rule with our name exists.
    Missing,
    /// Rule exists but its definition differs (port/scope drift).
    Drifted,
    /// The platform cannot report the rule state.
    Unsupported,
    /// Querying or changing the rule failed.
    Error(String),
}

#[cfg_attr(not(windows), allow(dead_code))] // exercised by Windows builds and tests
fn planned(argv: Vec<String>) -> PlannedCommand {
    PlannedCommand {
        argv,
        cwd: None,
        env: Default::default(),
        stdout_log: None,
        stderr_log: None,
    }
}

#[cfg_attr(not(windows), allow(dead_code))] // exercised by Windows builds and tests
fn netsh_show_argv(port: u16) -> Vec<String> {
    vec![
        "netsh".into(),
        "advfirewall".into(),
        "firewall".into(),
        "show".into(),
        "rule".into(),
        format!("name={}", rule_name(port)),
        "verbose".into(),
    ]
}

/// Read the current rule state.
pub async fn status(port: u16) -> FirewallStatus {
    #[cfg(windows)]
    {
        let ops = manager_wizard::ops::RealSystemOps::default();
        match ops.run_command(&planned(netsh_show_argv(port))).await {
            Ok(out) => {
                let text = out.stdout.to_lowercase();
                if text.contains("no rules match") {
                    FirewallStatus::Missing
                } else if text.contains(&format!("localport: {}", port))
                    && text.contains("100.64.0.0/10")
                    && text.contains("localsubnet")
                {
                    FirewallStatus::Ok
                } else {
                    FirewallStatus::Drifted
                }
            }
            Err(e) => FirewallStatus::Error(e.to_string()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        FirewallStatus::Unsupported
    }
}

/// Create or repair the rule. One elevation prompt when a change is
/// actually needed; a no-op when the rule already matches.
pub async fn ensure(port: u16) -> Result<(), String> {
    match status(port).await {
        FirewallStatus::Ok => return Ok(()),
        FirewallStatus::Missing => {}
        FirewallStatus::Drifted => {
            remove(port).await?;
        }
        FirewallStatus::Unsupported => return Ok(()),
        FirewallStatus::Error(e) => return Err(e),
    }
    #[cfg(windows)]
    {
        let ops = manager_wizard::ops::RealSystemOps::default();
        let argv = vec![
            "netsh".into(),
            "advfirewall".into(),
            "firewall".into(),
            "add".into(),
            "rule".into(),
            format!("name={}", rule_name(port)),
            "dir=in".into(),
            "action=allow".into(),
            "protocol=TCP".into(),
            format!("localport={port}"),
            format!("remoteip={REMOTE_SCOPES}"),
            "profile=any".into(),
        ];
        let out = ops
            .run_elevated(&planned(argv))
            .await
            .map_err(|e| e.to_string())?;
        if out.status == 0 {
            Ok(())
        } else {
            Err(format!(
                "netsh add rule failed ({}): {}",
                out.status,
                out.stderr.trim()
            ))
        }
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Remove the rule (on disable). One elevation prompt.
pub async fn remove(port: u16) -> Result<(), String> {
    #[cfg(windows)]
    {
        if matches!(status(port).await, FirewallStatus::Missing) {
            return Ok(());
        }
        let ops = manager_wizard::ops::RealSystemOps::default();
        let argv = vec![
            "netsh".into(),
            "advfirewall".into(),
            "firewall".into(),
            "delete".into(),
            "rule".into(),
            format!("name={}", rule_name(port)),
        ];
        let out = ops
            .run_elevated(&planned(argv))
            .await
            .map_err(|e| e.to_string())?;
        if out.status == 0 {
            Ok(())
        } else {
            Err(format!(
                "netsh delete rule failed ({}): {}",
                out.status,
                out.stderr.trim()
            ))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_identity() {
        assert_eq!(rule_name(47821), "LLMdil Admin API (47821)");
        assert!(REMOTE_SCOPES.contains("100.64.0.0/10"));
        assert!(netsh_show_argv(47821)
            .iter()
            .any(|a| a.contains("47821")));
    }

    #[tokio::test]
    async fn status_off_windows_is_unsupported_and_ensure_is_noop() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(status(47821).await, FirewallStatus::Unsupported);
        assert!(ensure(47821).await.is_ok());
        assert!(remove(47821).await.is_ok());
    }
}
