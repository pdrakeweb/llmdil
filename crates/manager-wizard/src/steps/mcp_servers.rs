//! Step 8: configure MCP servers.

use super::*;

// ---------------------------------------------------------------------------
// 8. Configure MCP servers
// ---------------------------------------------------------------------------

/// Credential Manager targets. Values are never read — presence only.
pub const GITHUB_CRED_TARGET: &str = "local-llm-service-manager/github-mcp";
pub const GDRIVE_CRED_TARGET: &str = "local-llm-service-manager/gdrive-mcp";

/// Step 8: verify MCP server credentials exist.
///
/// MCP Phase 1 (design §10.1): this step no longer writes
/// `<data_dir>/mcp/mcp-servers.json`. The persisted registry in
/// `AppConfig.mcp` is the source of truth now; the old file is
/// retired as a store, left in place if present (deleting user files
/// is not the migration's job), and noted as inert in the step log.
///
/// MCP Phase 4 (design §9): the presence check covers EVERY
/// registry server whose exposure is not `Disabled` and that names
/// a `cred_ref` — not only the two legacy servers. Presence only —
/// values are never read. A consequence, by design: a fresh wizard
/// run now fails this step while the seeded `brave_search` server
/// (Gateway-exposed, credential target
/// `local-llm-service-manager/brave-search-mcp`) has no stored
/// credential, until the operator stores one or sets the server
/// Disabled.
pub struct McpServers;

impl McpServers {
    /// Path of the retired `mcp-servers.json` store (design §10.1).
    /// Used only to note its presence as inert in the step log —
    /// nothing reads or writes it any more.
    fn retired_config_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mcp").join("mcp-servers.json")
    }
}

#[async_trait]
impl WizardStep for McpServers {
    fn id(&self) -> &str {
        STEP_MCP_SERVERS
    }
    fn name(&self) -> &str {
        "Configure MCP servers"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // File writes + credential presence checks; no process spawns.
        vec![]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("store the missing credential (MCP Servers view) or set the server Disabled, then re-run mcp-servers")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Credential presence check (design §9): every registry
        // server that is not Disabled and names a cred_ref. Values
        // are never read or shown — presence only. All missing
        // servers are collected (and logged) before failing, so one
        // run names every gap instead of only the first.
        let checked: Vec<&manager_config::McpServerConfig> = ctx
            .config
            .mcp
            .servers
            .iter()
            .filter(|s| {
                s.exposure != McpExposure::Disabled
                    && s.cred_ref
                        .as_deref()
                        .map(str::trim)
                        .map(|r| !r.is_empty())
                        .unwrap_or(false)
            })
            .collect();
        let mut missing: Vec<(&str, &str)> = Vec::new();
        for server in &checked {
            let target = server.cred_ref.as_deref().unwrap_or_default();
            match ctx.ops.credential_exists(target).await {
                Ok(true) => log.push(format!(
                    "mcp server '{}': credential '{target}' present",
                    server.id
                )),
                Ok(false) => {
                    log.push(format!(
                        "mcp server '{}': credential '{target}' missing",
                        server.id
                    ));
                    missing.push((server.id.as_str(), target));
                }
                Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
            }
        }
        if !missing.is_empty() {
            let named = missing
                .iter()
                .map(|(id, target)| format!("{id} ('{target}')"))
                .collect::<Vec<_>>()
                .join(", ");
            // The per-server present/missing log lines ride the
            // failure's log tail, ahead of the remediation.
            log.push(format!(
                "missing MCP server credential(s): {named} — store the credential (MCP Servers view), or set the server Disabled, then retry this step"
            ));
            log.push("credential values are never displayed or stored".to_string());
            return Ok(StepOutcome::failed(
                format!(
                    "MCP server credential(s) not found in Windows Credential Manager: {named}"
                ),
                log,
            ));
        }
        // Retired store (design §10.1): the step writes nothing. If a
        // pre-Phase-1 mcp-servers.json is still on disk, say plainly
        // in the success log that it is inert — left in place, and
        // nothing reads it.
        let retired = self.retired_config_path(ctx);
        if ctx.ops.file_exists(&retired).await {
            log.push(format!(
                "note: {} is present but inert — the retired mcp-servers.json store is no longer written or read; the persisted AppConfig.mcp registry is the source of truth",
                retired.display()
            ));
        } else {
            log.push(
                "note: the retired mcp-servers.json store is no longer written; the persisted AppConfig.mcp registry is the source of truth"
                    .to_string(),
            );
        }
        let checked_ids = checked
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(StepOutcome::done(
            format!("MCP server credentials present ({checked_ids})"),
            log,
        ))
    }
}
