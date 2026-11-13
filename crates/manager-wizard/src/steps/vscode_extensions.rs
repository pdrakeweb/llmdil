//! Step 7: install VS Code extensions and client config.

use super::*;

// ---------------------------------------------------------------------------
// 7. Install VS Code extensions
// ---------------------------------------------------------------------------

/// Step 7: install Continue.dev + Cline, write the Continue config pointing
/// at the local gateway.
pub struct VscodeExtensions {
    pub extensions: Vec<String>,
}

impl Default for VscodeExtensions {
    fn default() -> Self {
        Self {
            extensions: vec![
                "Continue.continue".to_string(),
                "saoudrizwan.claude-dev".to_string(),
                // The LiteLLM provider for Copilot Chat; its endpoint
                // and API key are configured by this step (see
                // `write_litellm_endpoint`).
                "vivswan.litellm-vscode-chat".to_string(),
            ],
        }
    }
}

/// True when a command error is the ops layer's spawn-level failure
/// (the executable could not be started at all), as opposed to a
/// program that ran and exited nonzero. The ops layer reports spawn
/// failures with exit code -1 and its own `spawn failed:` prefix.
/// Steps whose external tool is optional (VS Code, the MXC runtime)
/// skip instead of failing when their executable is absent.
pub(crate) fn spawn_failed(e: &WizardError) -> bool {
    matches!(
        e,
        WizardError::CommandFailed { code: -1, stderr, .. }
            if stderr.starts_with("spawn failed")
    )
}

impl VscodeExtensions {
    fn install_commands(&self, cli: &str) -> Vec<PlannedCommand> {
        self.extensions
            .iter()
            .map(|ext| {
                PlannedCommand::new(vec![
                    cli.to_string(),
                    "--install-extension".to_string(),
                    ext.clone(),
                    "--force".to_string(),
                ])
            })
            .collect()
    }

    /// See [`spawn_failed`].
    pub(crate) fn spawn_failed(e: &WizardError) -> bool {
        spawn_failed(e)
    }

    /// The skip outcome for a machine without VS Code: the extensions
    /// are optional editor tooling, so a missing `code` executable is
    /// a skip, never a setup failure.
    fn vscode_missing() -> StepOutcome {
        StepOutcome::skipped(
            "VS Code is not installed (the `code` command was not found); extension install skipped",
        )
    }

    /// The skip outcome when a VS Code install was located but its
    /// CLI still could not be started.
    fn vscode_unusable(cli: &str) -> StepOutcome {
        StepOutcome::skipped(format!(
            "VS Code was found at {cli} but its command-line tool could not be started; extension install skipped"
        ))
    }

    fn missing_or_unusable(resolved: bool, cli: &str) -> StepOutcome {
        if resolved {
            Self::vscode_unusable(cli)
        } else {
            Self::vscode_missing()
        }
    }

    /// VS Code user settings on Windows:
    /// `%APPDATA%\Code\User\settings.json`, resolved from the home
    /// directory (`%USERPROFILE%\AppData\Roaming\...`).
    fn vscode_settings_path(home: &Path) -> PathBuf {
        home.join("AppData")
            .join("Roaming")
            .join("Code")
            .join("User")
            .join("settings.json")
    }

    fn copilot_agents_dir(home: &Path) -> PathBuf {
        home.join(".copilot").join("agents")
    }

    fn copilot_agent_path(home: &Path, group: &str) -> PathBuf {
        Self::copilot_agents_dir(home).join(format!("litellm-{group}.md"))
    }

    /// Write the LiteLLM extension's server entry (endpoint + master
    /// key) into the VS Code user settings, preserving the rest of
    /// the file (see [`merge_litellm_server_setting`]). `Ok(false)`
    /// when no home directory is known and nothing was written.
    async fn write_litellm_endpoint(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<bool, StepOutcome> {
        let Some(home) = ctx.ops.home_dir() else {
            log.push("home directory unknown; LiteLLM endpoint not configured".to_string());
            return Ok(false);
        };
        let key = crate::ensure_gateway_master_key(ctx.ops.as_ref())
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!(
                        "could not obtain the gateway master key for the LiteLLM endpoint: {e}"
                    ),
                    vec![format!("{e:?}")],
                )
            })?;
        let path = Self::vscode_settings_path(&home);
        let existing = match ctx.ops.read_file(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => "{}".to_string(),
        };
        let entry = litellm_server_entry(ctx.config.gateway.port, &key);
        let merged = merge_litellm_server_setting(&existing, &entry).map_err(|e| {
            StepOutcome::failed(
                format!(
                    "could not update VS Code settings at {}: {e}",
                    path.display()
                ),
                vec!["the settings file was left unchanged".to_string()],
            )
        })?;
        ctx.ops
            .write_file(&path, merged.as_bytes())
            .await
            .map_err(|e| StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))?;
        log.push(format!(
            "LiteLLM endpoint configured in {} (server \"{LITELLM_SERVER_LABEL}\", base URL {})",
            path.display(),
            entry["baseUrl"].as_str().unwrap_or_default()
        ));
        Ok(true)
    }

    /// Write one Copilot custom-agent file per gateway group into
    /// `~/.copilot/agents/`. These files are manager-owned (Pete's
    /// requirement, 2026-10-15): every run rewrites them for the
    /// currently configured groups. Returns the number written (0
    /// when no home directory is known).
    async fn write_copilot_agents(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<usize, StepOutcome> {
        let Some(home) = ctx.ops.home_dir() else {
            log.push("home directory unknown; Copilot agents not written".to_string());
            return Ok(0);
        };
        let mut written = 0;
        for group in &ctx.config.gateway.groups {
            let path = Self::copilot_agent_path(&home, &group.name);
            let body = copilot_agent_markdown(&group.name);
            ctx.ops
                .write_file(&path, body.as_bytes())
                .await
                .map_err(|e| StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))?;
            log.push(format!("wrote Copilot agent {}", path.display()));
            written += 1;
        }
        Ok(written)
    }
}

#[async_trait]
impl WizardStep for VscodeExtensions {
    fn id(&self) -> &str {
        STEP_VSCODE_EXTENSIONS
    }
    fn name(&self) -> &str {
        "Install VS Code extensions"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        match ctx.mode {
            // The CLI shown here is the bare command; at runtime it
            // is resolved to an absolute path when the app process's
            // PATH cannot see the install (see `run`).
            RunMode::Install => self.install_commands("code"),
            // Audit only probes which extensions are present.
            RunMode::Audit => vec![PlannedCommand::new(vec![
                "code".to_string(),
                "--list-extensions".to_string(),
            ])],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run vscode-extensions to reinstall missing extensions")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Resolve the CLI before touching PATH: a GUI process
        // inherits the environment of whatever launched it, which
        // may predate the VS Code install (or the PATH edit its
        // installer made), so bare `code` can be unspawnable on a
        // machine where `code` works in a shell. The ops layer
        // resolves the install from the registry (App Paths) and
        // standard install locations; the bare command is the
        // fallback when nothing is found.
        let cli = ctx
            .ops
            .vscode_cli()
            .await
            .unwrap_or_else(|| "code".to_string());
        let resolved = cli != "code";
        match ctx.mode {
            RunMode::Install => {
                // Probe first: without the CLI there is nothing
                // to install into, and the step skips instead of
                // failing on the first extension.
                let probe = PlannedCommand::new(vec![cli.clone(), "--version".to_string()]);
                match ctx.ops.run_command(&probe).await {
                    Ok(_) => {}
                    Err(e) if Self::spawn_failed(&e) => {
                        return Ok(Self::missing_or_unusable(resolved, &cli))
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                for cmd in self.install_commands(&cli) {
                    log.push(format!("running: {}", cmd.argv.join(" ")));
                    match ctx.ops.run_command(&cmd).await {
                        Ok(out) if out.success() => {}
                        Ok(out) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "extension install failed (exit {}): {}",
                                    out.exit_code,
                                    cmd.argv.join(" ")
                                ),
                                vec![out.stderr],
                            ))
                        }
                        Err(e) if Self::spawn_failed(&e) => {
                            return Ok(Self::missing_or_unusable(resolved, &cli))
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                }
                // Continue: the config.yaml + .env pair (the legacy
                // config.json write is retired — see write_continue_files).
                let home = ctx.ops.home_dir();
                let continue_write = match write_continue_files(
                    ctx.ops.as_ref(),
                    &ctx.config,
                    home.as_deref(),
                    &ctx.data_dir,
                )
                .await
                {
                    Ok(w) => w,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                log.extend(continue_write.log);
                let continue_configured = continue_write.applied;
                let endpoint_configured = match self.write_litellm_endpoint(ctx, &mut log).await {
                    Ok(done) => done,
                    Err(outcome) => return Ok(outcome),
                };
                let agents_written = match self.write_copilot_agents(ctx, &mut log).await {
                    Ok(n) => n,
                    Err(outcome) => return Ok(outcome),
                };
                let mut message =
                    format!("{} VS Code extension(s) installed", self.extensions.len());
                if continue_configured {
                    message.push_str("; Continue configured");
                }
                if endpoint_configured {
                    message.push_str("; LiteLLM endpoint configured");
                }
                if agents_written > 0 {
                    message.push_str(&format!("; {agents_written} Copilot agent(s) written"));
                }
                Ok(StepOutcome::done(message, log))
            }
            RunMode::Audit => {
                let cmd = PlannedCommand::new(vec![cli.clone(), "--list-extensions".to_string()]);
                let out = match ctx.ops.run_command(&cmd).await {
                    Ok(o) => o,
                    Err(e) if Self::spawn_failed(&e) => {
                        return Ok(Self::missing_or_unusable(resolved, &cli))
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                let listed = out.stdout.to_lowercase();
                let missing: Vec<&str> = self
                    .extensions
                    .iter()
                    .map(String::as_str)
                    .filter(|ext| !listed.contains(&ext.to_lowercase()))
                    .collect();
                if !missing.is_empty() {
                    return Ok(StepOutcome::failed(
                        format!("missing VS Code extensions: {}", missing.join(", ")),
                        vec![],
                    ));
                }
                // Extensions are present; also verify the pieces the
                // install run writes: the LiteLLM endpoint entry in
                // the user settings and one Copilot agent file per
                // gateway group. Only checked when a home directory
                // is known — the audit verifies what it can reach.
                let mut problems: Vec<String> = Vec::new();
                if let Some(home) = ctx.ops.home_dir() {
                    let expected_base = format!("http://127.0.0.1:{}", ctx.config.gateway.port);
                    let settings_path = Self::vscode_settings_path(&home);
                    let entry_ok = match ctx.ops.read_file(&settings_path).await {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            jsonc_parser::parse_to_serde_value(
                                &text,
                                &jsonc_parser::ParseOptions::default(),
                            )
                            .ok()
                            .flatten()
                            .and_then(|v| {
                                v.get("litellm-vscode-chat.servers")
                                    .and_then(|s| s.as_array().cloned())
                            })
                            .map(|servers| {
                                servers.iter().any(|s| {
                                    s.get("label").and_then(|v| v.as_str())
                                        == Some(LITELLM_SERVER_LABEL)
                                        && s.get("baseUrl").and_then(|v| v.as_str())
                                            == Some(expected_base.as_str())
                                        && s.get("auth")
                                            .and_then(|a| a.get("apiKey"))
                                            .and_then(|k| k.as_str())
                                            .map(|k| !k.is_empty())
                                            == Some(true)
                                })
                            })
                            .unwrap_or(false)
                        }
                        Err(_) => false,
                    };
                    if !entry_ok {
                        problems.push(
                            "LiteLLM endpoint not configured in VS Code settings".to_string(),
                        );
                    }
                    let mut missing_agents: Vec<String> = Vec::new();
                    for group in &ctx.config.gateway.groups {
                        let path = Self::copilot_agent_path(&home, &group.name);
                        if !ctx.ops.file_exists(&path).await {
                            missing_agents.push(group.name.clone());
                        }
                    }
                    if !missing_agents.is_empty() {
                        problems.push(format!(
                            "missing Copilot agent file(s): {}",
                            missing_agents.join(", ")
                        ));
                    }
                    // Continue: config.yaml must carry every gateway
                    // group with the env-referenced key, and .env
                    // must hold the key that reference resolves.
                    let cont_dir = home.join(".continue");
                    // Continue's apiBase carries /v1 (unlike the
                    // extension endpoint above, which must not).
                    let expected_api_base = format!("{expected_base}/v1");
                    let expected_key_ref = "${{ secrets.LITELLM_API_KEY }}";
                    let yaml_models: Vec<serde_yaml::Value> =
                        match ctx.ops.read_file(&cont_dir.join("config.yaml")).await {
                            Ok(bytes) => {
                                let text = String::from_utf8_lossy(&bytes).into_owned();
                                serde_yaml::from_str::<serde_yaml::Value>(&text)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("models").and_then(|m| m.as_sequence().cloned())
                                    })
                                    .unwrap_or_default()
                            }
                            Err(_) => Vec::new(),
                        };
                    let missing_models: Vec<String> = ctx
                        .config
                        .gateway
                        .groups
                        .iter()
                        .filter(|g| {
                            !yaml_models.iter().any(|m| {
                                m.get("model").and_then(|v| v.as_str()) == Some(g.name.as_str())
                                    && m.get("apiBase").and_then(|v| v.as_str())
                                        == Some(expected_api_base.as_str())
                                    && m.get("apiKey").and_then(|v| v.as_str())
                                        == Some(expected_key_ref)
                            })
                        })
                        .map(|g| g.name.clone())
                        .collect();
                    if !missing_models.is_empty() {
                        problems.push(format!(
                            "Continue config.yaml missing gateway model(s): {}",
                            missing_models.join(", ")
                        ));
                    }
                    let env_ok = match ctx.ops.read_file(&cont_dir.join(".env")).await {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            text.lines().any(|line| {
                                line.trim_start()
                                    .strip_prefix("export ")
                                    .unwrap_or(line.trim_start())
                                    .strip_prefix("LITELLM_API_KEY=")
                                    .map(|v| !v.trim().is_empty())
                                    .unwrap_or(false)
                            })
                        }
                        Err(_) => false,
                    };
                    if !env_ok {
                        problems.push("Continue .env missing LITELLM_API_KEY".to_string());
                    }
                }
                if problems.is_empty() {
                    Ok(StepOutcome::done(
                        format!("{} extension(s) present", self.extensions.len()),
                        vec![],
                    ))
                } else {
                    Ok(StepOutcome::failed(
                        format!("VS Code setup incomplete: {}", problems.join("; ")),
                        vec![],
                    ))
                }
            }
        }
    }
}
