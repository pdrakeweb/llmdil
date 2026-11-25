//! Step 6: start the LiteLLM gateway.

use super::*;

mod health;
mod install;
mod launch;
mod prisma;
mod process;
mod venv;

pub use self::launch::*;

// ---------------------------------------------------------------------------
// 6. Start LiteLLM gateway
// ---------------------------------------------------------------------------

/// Budget for the quick verification probes (interpreter version,
/// package import, package version): each is a version print or an
/// import, done in seconds when healthy. They previously ran at
/// the ops layer's generic 600 s command budget, so a wedged probe
/// held the step silently for ten minutes before any repair was
/// even chosen.
pub(crate) const PROBE_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Step 6: bootstrap the managed Python env (uv), generate `config.yaml`
/// from the app config, spawn LiteLLM on :4000, health-check.
pub struct StartGateway {
    pub health_retries: u32,
    pub health_interval_ms: u64,
    /// How long the database path keeps confirming health after the
    /// first healthy probe before the step reports success
    /// ([`StartGateway::confirm_sustained_health`]). Zero disables
    /// the confirmation window (tests); production uses the
    /// default.
    pub db_sustain_ms: u64,
}

impl Default for StartGateway {
    fn default() -> Self {
        Self {
            // Two minutes: a cold LiteLLM start (interpreter + full
            // dependency import on a fresh venv, under antivirus
            // scanning) routinely exceeds the previous 30 s budget.
            // A process that exits is detected within one interval by
            // the liveness check in wait_healthy, so the larger budget
            // costs nothing on real failures.
            health_retries: 120,
            health_interval_ms: 1000,
            // Both known gateway-with-database defects strike about
            // a second after a plain health probe first succeeds —
            // the gateway answers, then dies. Ten seconds of
            // continued answers is far past that window.
            db_sustain_ms: 10_000,
        }
    }
}

/// Escape a value for interpolation inside a PowerShell
/// double-quoted string. PowerShell's escape character is the
/// backtick: `` ` `` doubles itself, and backtick before `"` or `$`
/// stops the quote ending the string and the dollar starting a
/// variable/subexpression expansion. A backslash escapes nothing
/// in PowerShell — the previous `\"` rendering of an embedded
/// quote actually TERMINATED the string (code-review 2.7).
fn ps_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '`' => out.push_str("``"),
            '"' => out.push_str("`\""),
            '$' => out.push_str("`$"),
            _ => out.push(c),
        }
    }
    out
}

/// Join an argv into one paste-able PowerShell command line:
/// arguments containing whitespace, a quote, a backtick, or a `$`
/// are double-quoted with their contents escaped via [`ps_dq`];
/// everything else passes through bare.
fn ps_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() {
                "\"\"".to_string()
            } else if a
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '`' || c == '$')
            {
                format!("\"{}\"", ps_dq(a))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render one executable command line for the manual start block.
/// PowerShell parses a line whose first token is a quoted string as
/// an expression, not a command, and rejects it ("Unexpected token")
/// — the call operator `&` is required before a quoted executable
/// path, and harmless before a bare one, so every executable line
/// in the block carries it unconditionally. (An operator pasting
/// the block by hand hit exactly this, 2026-10-16.)
pub(crate) fn ps_command_line(argv: &[String]) -> String {
    format!("& {}", ps_join(argv))
}

#[async_trait]
impl WizardStep for StartGateway {
    fn id(&self) -> &str {
        STEP_START_GATEWAY
    }
    fn name(&self) -> &str {
        "Start LiteLLM gateway"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![
            STEP_CONFIGURE_TENSOR_SPLIT.to_string(),
            STEP_DOWNLOAD_MODELS.to_string(),
        ]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        match ctx.mode {
            // The commands run() may spawn, in order (plus the uv
            // bootstrap download when uv is not yet resolvable). The
            // venv probes always run; the venv/pip commands are skipped
            // when the probes show a current venv with litellm installed.
            RunMode::Install => {
                let (uv, needs_bootstrap) = Self::uv_argv0(ctx);
                let mut cmds = Vec::new();
                if needs_bootstrap {
                    let url = crate::uv_download_url()
                        .expect("needs_bootstrap is only true when a download URL exists");
                    let archive = Self::uv_bin_dir(ctx).join(Self::uv_archive_name());
                    cmds.push(PlannedCommand::new(vec![
                        "download".to_string(),
                        url,
                        archive.display().to_string(),
                    ]));
                }
                cmds.push(self.version_probe_command(ctx));
                cmds.push(self.litellm_probe_command(ctx));
                cmds.push(self.venv_command(ctx, &uv));
                cmds.push(self.pip_command(ctx, &uv));
                let mut spawn = self.spawn_command(ctx);
                // The gateway master key is always set at spawn (read
                // from — or first generated into — Credential
                // Manager); the plan only records that the variable
                // will be set. The value never appears here.
                spawn.env.insert(
                    "LITELLM_MASTER_KEY".to_string(),
                    "<set from credential manager>".to_string(),
                );
                let tier = &ctx.config.gateway.openrouter;
                if tier.enabled && tier.cred_ref.as_deref().is_some_and(|r| !r.is_empty()) {
                    // The real key is read from Credential Manager when
                    // the step runs; the plan only records that the
                    // variable will be set. The value never appears here.
                    spawn.env.insert(
                        "OPENROUTER_API_KEY".to_string(),
                        "<set from credential manager>".to_string(),
                    );
                }
                // MCP gateway servers (design §4.2): planned parity
                // with the spawned env, following the OpenRouter
                // precedent exactly — decided from the config alone
                // (a plan performs no credential reads), placeholders
                // never values. The stdio flag is planned when any
                // Gateway-exposed stdio server is configured; if its
                // credential cannot be read at run time the server
                // is not armed and the spawned env omits its entries.
                let gateway_mcp: Vec<&manager_config::McpServerConfig> = ctx
                    .config
                    .mcp
                    .servers
                    .iter()
                    .filter(|s| s.exposure == manager_config::McpExposure::Gateway)
                    .collect();
                if gateway_mcp
                    .iter()
                    .any(|s| s.transport == manager_config::McpTransport::Stdio)
                {
                    spawn
                        .env
                        .insert("LITELLM_ENABLE_MCP_STDIO".to_string(), "true".to_string());
                }
                for server in &gateway_mcp {
                    if server.cred_ref.as_deref().is_some_and(|r| !r.is_empty()) {
                        for key in &server.env_keys {
                            spawn
                                .env
                                .insert(key.clone(), "<set from credential manager>".to_string());
                        }
                    }
                }
                // SearXNG (design §6.3): planned parity with the
                // spawned env. The api_base is not a secret — it is
                // config — so the plan shows the real value, the way
                // spawn_command's own non-secret env values
                // (PYTHONIOENCODING et al.) are shown.
                let searxng = &ctx.config.mcp.searxng;
                if searxng.enabled && !searxng.api_base.trim().is_empty() {
                    spawn.env.insert(
                        "SEARXNG_API_BASE".to_string(),
                        searxng.api_base.trim().to_string(),
                    );
                }
                cmds.push(spawn);
                // Firewall reconciliation for the bind address: a
                // read-only rule query, plus the elevated script
                // when the rule state does not match the bind.
                cmds.push(self.firewall_show_command(ctx.config.gateway.port));
                cmds.push(self.firewall_elevated_command(ctx));
                cmds
            }
            // Audit only re-probes the health endpoint; nothing is spawned.
            RunMode::Audit => vec![PlannedCommand::new(vec![
                "http-get".to_string(),
                self.gateway_url(ctx),
            ])],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run start-gateway to regenerate config and restart the gateway")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        match ctx.mode {
            RunMode::Install => self.run_install_marked(ctx).await,
            RunMode::Audit => {
                // Re-probe backend health without reinstalling.
                let headers = self.gateway_auth_headers(ctx).await;
                match ctx
                    .ops
                    .http_get_with_headers(&self.gateway_url(ctx), &headers)
                    .await
                {
                    Ok(resp) if resp.status == 200 => Ok(StepOutcome::done(
                        format!("gateway healthy on :{}", ctx.config.gateway.port),
                        vec![format!("GET /v1/models → 200 in {} ms", resp.elapsed_ms)],
                    )),
                    Ok(resp) => Ok(StepOutcome::failed(
                        format!("gateway unhealthy: HTTP {}", resp.status),
                        vec![],
                    )),
                    Err(e) => Ok(StepOutcome::failed(
                        format!("gateway unreachable: {e}"),
                        vec![format!("{e:?}")],
                    )),
                }
            }
        }
    }
}
