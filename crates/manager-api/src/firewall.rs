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

/// Split one `netsh ... show rule ... verbose` output line into a
/// `(field, value)` pair.
///
/// netsh prints values in a fixed-width padded column (`LocalPort:`
/// followed by ~28 spaces, then the value), so a plain substring
/// like `localport: 47821` can never match — that single-space
/// assumption is what made every existing rule read as Drifted.
/// Split at the first `:` and trim both sides. Lines without a
/// colon (separator rows, `Ok.`) yield nothing.
#[cfg_attr(not(windows), allow(dead_code))] // exercised by Windows builds and tests
fn parse_field(line: &str) -> Option<(&str, &str)> {
    let (field, value) = line.split_once(':')?;
    let field = field.trim();
    if field.is_empty() {
        return None;
    }
    Some((field, value.trim()))
}

/// Classify a verbose `netsh` dump for `port` without running
/// netsh. Pure, so the regression test can feed it verbatim dumps.
/// The input is lowercased here, so all comparisons are
/// case-insensitive. Any field we cannot confirm makes the rule
/// Drifted (fail closed); only a fully matching rule is Ok.
#[cfg_attr(not(windows), allow(dead_code))] // exercised by Windows builds and tests
fn classify_output(raw: &str, port: u16) -> FirewallStatus {
    let text = raw.to_lowercase();
    if text.contains("no rules match") {
        return FirewallStatus::Missing;
    }
    let want_port = port.to_string();
    let mut local_port: Option<&str> = None;
    let mut remote_ip: Option<&str> = None;
    let mut direction: Option<&str> = None;
    let mut action: Option<&str> = None;
    let mut profiles: Option<&str> = None;
    for line in text.lines() {
        let Some((field, value)) = parse_field(line) else {
            continue;
        };
        // First occurrence wins: `show rule name=...` can return
        // several same-named rules; the first block is the rule
        // netsh lists for the name.
        match field {
            "localport" if local_port.is_none() => local_port = Some(value),
            "remoteip" if remote_ip.is_none() => remote_ip = Some(value),
            "direction" if direction.is_none() => direction = Some(value),
            "action" if action.is_none() => action = Some(value),
            "profiles" if profiles.is_none() => profiles = Some(value),
            _ => {}
        }
    }
    let port_ok = local_port == Some(want_port.as_str());
    let remote_ok = remote_ip.is_some_and(|v| {
        let scopes: Vec<&str> = v.split(',').map(str::trim).collect();
        scopes.contains(&"localsubnet") && scopes.contains(&"100.64.0.0/10")
    });
    let direction_ok = direction == Some("in");
    let action_ok = action == Some("allow");
    let profiles_ok = profiles.is_some_and(|v| {
        let have: Vec<&str> = v.split(',').map(str::trim).collect();
        ["domain", "private", "public"]
            .iter()
            .all(|p| have.contains(p))
    });
    if port_ok && remote_ok && direction_ok && action_ok && profiles_ok {
        FirewallStatus::Ok
    } else {
        FirewallStatus::Drifted
    }
}

/// Read the current rule state.
pub async fn status(port: u16) -> FirewallStatus {
    #[cfg(windows)]
    {
        let ops = manager_wizard::ops::RealSystemOps::default();
        match ops.run_command(&planned(netsh_show_argv(port))).await {
            Ok(out) => classify_output(&out.stdout, port),
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
        if out.exit_code == 0 {
            Ok(())
        } else {
            Err(format!(
                "netsh add rule failed ({}): {}",
                out.exit_code,
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
        if out.exit_code == 0 {
            Ok(())
        } else {
            Err(format!(
                "netsh delete rule failed ({}): {}",
                out.exit_code,
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

    /// Verbatim `netsh advfirewall firewall show rule ... verbose`
    /// dump layout: values start at column 38 (fixed-width padded
    /// column), copied from the Windows Server 2025 / Windows 11
    /// 24H2 sample at
    /// https://computingforgeeks.com/windows-cmd-commands-cheat-sheet/
    /// (captured Oct 2026; the same layout appears in samples back
    /// to Windows 7). The field values are this rule's (name, port
    /// 47821, RemoteIP LocalSubnet,100.64.0.0/10); the
    /// LocalPort/RemoteIP lines match the diagnosis report's
    /// verbatim lines.
    const VERBATIM_DUMP: &str = r#"Rule Name:                            LLMdil Admin API (47821)
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Domain,Private,Public
Grouping:
LocalIP:                              Any
RemoteIP:                             LocalSubnet,100.64.0.0/10
Protocol:                             TCP
LocalPort:                            47821
RemotePort:                           Any
Edge traversal:                       No
Action:                               Allow
Ok.
"#;

    /// Re-emit one field line of a dump at the same column-38
    /// layout. Used only to derive drifted variants from the
    /// verbatim fixture above (those variants are constructed,
    /// not verbatim).
    fn with_field(dump: &str, field: &str, value: &str) -> String {
        dump.lines()
            .map(|line| {
                let is_field = line
                    .split_once(':')
                    .is_some_and(|(f, _)| f.trim().eq_ignore_ascii_case(field));
                if is_field {
                    format!("{:<38}{}", format!("{field}:"), value)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn status_ok_on_verbatim_padded_dump() {
        assert_eq!(classify_output(VERBATIM_DUMP, 47821), FirewallStatus::Ok);
    }

    #[test]
    fn status_drifted_when_port_differs() {
        let dump = with_field(VERBATIM_DUMP, "LocalPort", "47822");
        assert_eq!(classify_output(&dump, 47821), FirewallStatus::Drifted);
    }

    #[test]
    fn status_drifted_when_scope_narrow() {
        let dump = with_field(VERBATIM_DUMP, "RemoteIP", "LocalSubnet");
        assert_eq!(classify_output(&dump, 47821), FirewallStatus::Drifted);
    }

    #[test]
    fn status_drifted_when_profile_not_any() {
        let dump = with_field(VERBATIM_DUMP, "Profiles", "Private");
        assert_eq!(classify_output(&dump, 47821), FirewallStatus::Drifted);
    }

    #[test]
    fn status_missing_when_no_rules_match() {
        assert_eq!(
            classify_output("No rules match the specified criteria.\n", 47821),
            FirewallStatus::Missing
        );
    }
}
