//! `StartGateway`: firewall rule, spawn command, master key, stopping the previous gateway, service spawn, and logs.

use super::*;

/// The app's gateway firewall rules are named `<prefix><port>)`.
const FIREWALL_RULE_PREFIX: &str = "Local LLM Gateway (TCP ";

/// The ports of this app's gateway firewall rules in a
/// `netsh advfirewall firewall show rule name=all` listing. Matched on
/// the exact rule name at the end of a line (netsh localizes the
/// "Rule Name:" label, never the name), so another program's rule is
/// never picked up.
pub(crate) fn gateway_firewall_rule_ports(listing: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = listing
        .lines()
        .filter_map(|line| {
            let name = line.split_once(':')?.1.trim();
            name.strip_prefix(FIREWALL_RULE_PREFIX)?
                .strip_suffix(')')?
                .parse()
                .ok()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

impl StartGateway {
    /// The exact bootstrap the venv's `litellm` console script runs
    /// (verified against the litellm 1.105.1 wheel's entry_points.txt:
    /// `litellm = litellm:run_server`, a click command that parses
    /// sys.argv[1:] when called with no arguments). Running it via
    /// `-c` invokes the same entry point with our arguments in
    /// sys.argv[1:], without the launcher process in between.
    /// `python -m litellm` is not an alternative — the package ships
    /// no `__main__` and exits immediately (found on real hardware
    /// 2026-10-15).
    pub(crate) const GATEWAY_BOOTSTRAP: &str =
        "import sys; from litellm import run_server; sys.exit(run_server())";

    /// The Windows firewall rule that admits LAN clients to the
    /// gateway: inbound TCP on the gateway port, Private profile
    /// only, remote addresses limited to the local subnet.
    fn firewall_rule_name(port: u16) -> String {
        format!("{FIREWALL_RULE_PREFIX}{port})")
    }

    pub(crate) fn firewall_show_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "netsh".to_string(),
            "advfirewall".to_string(),
            "firewall".to_string(),
            "show".to_string(),
            "rule".to_string(),
            format!("name={}", Self::firewall_rule_name(ctx.config.gateway.port)),
        ])
    }

    fn firewall_script_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("gateway").join("firewall.cmd")
    }

    pub(crate) fn firewall_elevated_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            self.firewall_script_path(ctx).display().to_string(),
        ])
    }

    /// The elevated script: delete the rule for this port and every
    /// stale one, then (for a LAN bind) add this port's rule.
    fn firewall_script_contents(&self, ctx: &WizardContext, add: bool, stale: &[u16]) -> String {
        let port = ctx.config.gateway.port;
        let mut script = "@echo off\r\n".to_string();
        for p in stale.iter().chain([&port]) {
            script.push_str(&format!(
                "netsh advfirewall firewall delete rule name=\"{}\" >nul 2>&1\r\n",
                Self::firewall_rule_name(*p)
            ));
        }
        if add {
            script.push_str(&format!(
                "netsh advfirewall firewall add rule name=\"{}\" dir=in action=allow protocol=TCP localport={port} profile=private remoteip=localsubnet\r\n",
                Self::firewall_rule_name(port)
            ));
        }
        script
    }

    /// Ports, other than the configured one, that still have a rule
    /// this app created (recognized by the exact rule name).
    async fn stale_firewall_ports(&self, ctx: &WizardContext) -> Vec<u16> {
        let list = PlannedCommand::new(
            [
                "netsh",
                "advfirewall",
                "firewall",
                "show",
                "rule",
                "name=all",
            ]
            .map(str::to_string)
            .to_vec(),
        );
        match ctx.ops.run_command(&list).await {
            Ok(out) => gateway_firewall_rule_ports(&out.stdout)
                .into_iter()
                .filter(|p| *p != ctx.config.gateway.port)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn firewall_rule_exists(&self, ctx: &WizardContext) -> bool {
        matches!(
            ctx.ops.run_command(&self.firewall_show_command(ctx)).await,
            Ok(out) if out.exit_code == 0
        )
    }

    /// Reconcile the firewall rule with the bind configuration: a
    /// LAN bind needs the rule, a loopback bind removes it. The
    /// rule query is unprivileged, so a matching state costs no UAC
    /// prompt; a mismatch runs one elevated script, which also removes
    /// the rules this app left for earlier ports. A declined or
    /// failed elevation is a warning, never a step failure — the
    /// gateway itself is already healthy, and loopback clients are
    /// unaffected by the rule either way.
    pub(crate) async fn ensure_gateway_firewall(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Option<String> {
        let want_rule = ctx.config.gateway.bind_host != "127.0.0.1";
        let have_rule = self.firewall_rule_exists(ctx).await;
        if want_rule && have_rule {
            log.push("Windows firewall rule for LAN gateway access already present".to_string());
            return None;
        }
        // Listing every rule takes a second or two, so only when this
        // port's rule does not settle the question.
        let stale = self.stale_firewall_ports(ctx).await;
        if !want_rule && !have_rule && stale.is_empty() {
            log.push("gateway binds loopback only; no firewall rule needed".to_string());
            return None;
        }
        push_log(
            ctx,
            log,
            "Windows will ask for administrator approval to update the firewall rule for the gateway"
                .to_string(),
        );
        let script = self.firewall_script_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(
                &script,
                self.firewall_script_contents(ctx, want_rule, &stale)
                    .as_bytes(),
            )
            .await
        {
            return Some(format!(
                "could not write the firewall script {}: {e}",
                script.display()
            ));
        }
        match ctx
            .ops
            .run_elevated(&self.firewall_elevated_command(ctx))
            .await
        {
            Ok(_) => {
                log.push(if want_rule {
                    format!(
                        "Windows firewall rule added: inbound TCP {} (Private profile, local subnet)",
                        ctx.config.gateway.port
                    )
                } else {
                    "Windows firewall rule for LAN gateway access removed".to_string()
                });
                if !stale.is_empty() {
                    log.push(format!(
                        "removed the firewall rule(s) for earlier gateway ports: {}",
                        stale
                            .iter()
                            .map(u16::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                None
            }
            Err(e) => Some(format!(
                "the Windows firewall rule was not {} ({e}); {}",
                if want_rule { "added" } else { "removed" },
                if want_rule {
                    "clients on the local network may be unable to connect until the rule exists"
                } else {
                    "a stale allow rule for the gateway port remains, inert while the gateway binds loopback"
                }
            )),
        }
    }

    pub(crate) fn spawn_command(&self, ctx: &WizardContext) -> PlannedCommand {
        let cfg = ctx.data_dir.join("gateway").join("config.yaml");
        let mut cmd = PlannedCommand::new(vec![
            self.gateway_python(ctx).display().to_string(),
            "-c".to_string(),
            Self::GATEWAY_BOOTSTRAP.to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
            // The configured bind address (default 0.0.0.0 — Pete's
            // decision 2026-10-15: other machines on the local
            // network use the gateway; 127.0.0.1 restricts it to this
            // computer). Authentication is the master key on every
            // interface.
            "--host".to_string(),
            ctx.config.gateway.bind_host.clone(),
            "--port".to_string(),
            ctx.config.gateway.port.to_string(),
        ]);
        // Force UTF-8 stdio in the gateway process. With stdout
        // redirected (the captured log file), Python on Windows
        // encodes output with the ANSI code page (cp1252); LiteLLM's
        // startup banner contains box-drawing characters cp1252
        // cannot represent, the print raises UnicodeEncodeError, and
        // application startup fails (found on real hardware
        // 2026-10-15 — a manual console run worked because a real
        // console takes the Unicode path). PYTHONIOENCODING targets
        // stdio; PYTHONUTF8 also covers the filesystem encoding.
        cmd.env
            .insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());
        cmd.env.insert("PYTHONUTF8".to_string(), "1".to_string());
        cmd
    }

    /// The spawn command plus the OpenRouter key environment. The
    /// generated YAML references `os.environ/OPENROUTER_API_KEY`; the key
    /// itself lives in Windows Credential Manager under the tier's
    /// `cred_ref` and is read here — it is never logged and never
    /// written into the YAML. When the tier is enabled but the key
    /// cannot be read, the gateway still starts and the returned warning
    /// states the OpenRouter fallback is not armed.
    /// The gateway master key, generating and storing one on first
    /// use. Stored in Windows Credential Manager under
    /// [`crate::LITELLM_MASTER_KEY_CRED_TARGET`]; an existing key is
    /// always reused, so clients already configured with it (editor
    /// configs the wizard wrote) keep working across restarts.
    async fn ensure_master_key(&self, ctx: &WizardContext) -> Result<String, StepOutcome> {
        crate::ensure_gateway_master_key(ctx.ops.as_ref())
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!(
                        "could not store the gateway master key in Windows Credential Manager: {e}"
                    ),
                    vec![format!("{e:?}")],
                )
            })
    }

    /// The master key for authenticating this app's own calls to the
    /// gateway (health probes, smoke tests), when one is stored.
    /// Never generates: probe paths must not create credentials.
    async fn stored_master_key(&self, ctx: &WizardContext) -> Option<String> {
        ctx.ops
            .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
            .filter(|k| !k.is_empty())
    }

    /// The Authorization header for this app's own gateway calls,
    /// when a master key is stored. A gateway that requires a key was
    /// started by a run of this step, which stored the key first, so
    /// the header is present exactly when the gateway demands it.
    pub(crate) async fn gateway_auth_headers(&self, ctx: &WizardContext) -> Vec<(String, String)> {
        match self.stored_master_key(ctx).await {
            Some(key) => vec![("Authorization".to_string(), format!("Bearer {key}"))],
            None => Vec::new(),
        }
    }

    /// Whether the gateway spawn must carry
    /// `PRISMA_HEALTH_WATCHDOG_ENABLED=false`. LiteLLM's Prisma
    /// health watchdog (`_poll_engine_proc` in the proxy utils,
    /// introduced by LiteLLM PR #21899) probes the query-engine
    /// process with `os.kill(pid, 0)`. On Linux/macOS signal 0 is a
    /// harmless existence probe; on Windows CPython routes
    /// `os.kill` to `TerminateProcess`, so the watchdog's first poll
    /// KILLS the engine it meant to check. The proxy reads the dead
    /// engine as a lost database connection and shuts Uvicorn down
    /// cleanly about a second after startup, logging no error —
    /// which is also why a standalone Prisma client (no watchdog)
    /// stays connected indefinitely. BerriAI/litellm issues #25260,
    /// #30847, #26886; field-verified 2026-10-16: with the flag set
    /// the gateway stays up. The watchdog only exists on the
    /// database path, and the defect is Windows-only, so the flag
    /// is set exactly then; elsewhere the watchdog works as
    /// intended and stays enabled. Downside of disabling it: no
    /// automatic engine restart if PostgreSQL or the engine dies
    /// later — the gateway needs a restart to recover its database,
    /// acceptable for an app-supervised single-node gateway.
    pub(crate) fn prisma_watchdog_must_be_disabled(
        on_windows: bool,
        database_url: Option<&str>,
    ) -> bool {
        on_windows && database_url.is_some()
    }

    pub(crate) async fn spawn_command_with_env(
        &self,
        ctx: &WizardContext,
        database_url: Option<&str>,
        mcp_env: &[(String, String)],
        mcp_stdio_enabled: bool,
    ) -> Result<(PlannedCommand, Option<String>), StepOutcome> {
        let mut cmd = self.spawn_command(ctx);
        // The master key is unconditional: current LiteLLM refuses
        // to boot without one. Failing to obtain it fails the step
        // here rather than spawning a gateway that exits at startup.
        let master_key = self.ensure_master_key(ctx).await?;
        cmd.env.insert("LITELLM_MASTER_KEY".to_string(), master_key);
        // MCP gateway servers (design §4.2): the stdio opt-in flag
        // when at least one armed stdio Gateway server exists, and
        // each armed server's credential under each of its env-key
        // names (the generated YAML references them as
        // os.environ/<NAME>). The values were read once by the
        // arming pass in run(); they are never logged.
        if mcp_stdio_enabled {
            cmd.env
                .insert("LITELLM_ENABLE_MCP_STDIO".to_string(), "true".to_string());
        }
        for (key, value) in mcp_env {
            cmd.env.insert(key.clone(), value.clone());
        }
        // SearXNG search provider (design §6.3): LiteLLM reads the
        // instance URL from SEARXNG_API_BASE in the process
        // environment (the generated YAML's search_tools entry
        // carries the same value as its api_base). The base is
        // config, not a secret, so it travels in plain form like
        // the other non-secret spawn values.
        let searxng = &ctx.config.mcp.searxng;
        if searxng.enabled && !searxng.api_base.trim().is_empty() {
            cmd.env.insert(
                "SEARXNG_API_BASE".to_string(),
                searxng.api_base.trim().to_string(),
            );
        }
        // The database URL is present exactly when the app-managed
        // PostgreSQL is installed and running (the caller started
        // it). With it, LiteLLM migrates its schema at startup and
        // the admin UI can be logged into; without it the gateway
        // runs database-free, as before PostgreSQL support.
        if let Some(url) = database_url {
            cmd.env.insert("DATABASE_URL".to_string(), url.to_string());
            // LiteLLM's Prisma health watchdog kills its own query
            // engine on Windows (see
            // Self::prisma_watchdog_must_be_disabled); with a
            // database configured it must not run.
            if Self::prisma_watchdog_must_be_disabled(cfg!(target_os = "windows"), database_url) {
                cmd.env.insert(
                    "PRISMA_HEALTH_WATCHDOG_ENABLED".to_string(),
                    "false".to_string(),
                );
            }
        }
        let tier = &ctx.config.gateway.openrouter;
        if !tier.enabled {
            return Ok((cmd, None));
        }
        let Some(cred_ref) = tier.cred_ref.as_deref().filter(|r| !r.is_empty()) else {
            return Ok((
                cmd,
                Some(
                    "OpenRouter fallback is not armed: the cloud tier is enabled but no credential reference is configured"
                        .to_string(),
                ),
            ));
        };
        match ctx.ops.credential_read(cred_ref).await {
            Some(key) => {
                cmd.env.insert("OPENROUTER_API_KEY".to_string(), key);
                Ok((cmd, None))
            }
            None => Ok((
                cmd,
                Some(format!(
                    "OpenRouter fallback is not armed: credential '{cred_ref}' could not be read from Windows Credential Manager"
                )),
            )),
        }
    }

    /// Stop the previous gateway, whatever runs it.
    ///
    /// Restart semantics: the previously spawned LiteLLM still holds
    /// the port and serves the old generated config, so spawning a
    /// second one would fail to bind (silently — its stdio is nulled)
    /// while the health check passes against the old process. Stopping
    /// it first is what makes re-running this step a real restart. A
    /// live previous gateway that cannot be stopped fails the step:
    /// failing loudly beats a duplicate spawn.
    ///
    /// Both kinds of previous gateway are handled on every run, so a
    /// supervision-mode flip or a (de)registration never leaves a
    /// duplicate behind (SCM-7, CR-4):
    /// 1. The gateway SERVICE, when it is registered (always on the
    ///    service route; on the process route only when it is
    ///    actually running), via the shared SCM stop-and-wait. The
    ///    typed outcome is logged as it happened (CR-7): a service
    ///    that was not running is not reported as stopped, and the
    ///    pid file is only cleared when the service owned it.
    /// 2. A live gateway PROCESS — the pid file's pid, else the
    ///    gateway interpreter owning the gateway port — killed and
    ///    awaited until it is gone and has released the port.
    pub(crate) async fn stop_previous_gateway(
        &self,
        ctx: &WizardContext,
        service_route: bool,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        use manager_services::{ServiceState, StopOutcome};
        let pid_path = crate::gateway_pid_path(&ctx.data_dir);
        let port = ctx.config.gateway.port;
        let service_name = manager_services::GATEWAY_SERVICE_NAME;
        // 1. The service. On the process route a registered service
        // is only stopped when it is actually up (a mode flip from
        // service to process while it runs); its child would
        // otherwise keep the port, and killing that child by pid
        // would leave the SCM state lying.
        let stop_service = if service_route {
            true
        } else if ctx.ops.service_registered(service_name).await {
            !matches!(
                ctx.ops.service_state(service_name).await,
                None | Some(ServiceState::Stopped) | Some(ServiceState::NotInstalled)
            )
        } else {
            false
        };
        if stop_service {
            let running_before = !matches!(
                ctx.ops.service_state(service_name).await,
                Some(ServiceState::Stopped) | Some(ServiceState::NotInstalled)
            );
            if running_before {
                gateway_mark(ctx, "stopping previous gateway", Some("service"));
            }
            match ctx.ops.service_stop(service_name).await {
                Ok(StopOutcome::NotRegistered) => {
                    push_log(
                        ctx,
                        log,
                        format!(
                            "gateway service '{service_name}' is not registered — no service to stop"
                        ),
                    );
                }
                Ok(StopOutcome::AlreadyStopped) => {
                    push_log(ctx, log, "gateway service was not running".to_string());
                }
                Ok(StopOutcome::Stopped { waited_ms }) => {
                    push_log(
                        ctx,
                        log,
                        format!("stopped previous gateway service in {waited_ms} ms"),
                    );
                    gateway_mark(
                        ctx,
                        "previous gateway stopped",
                        Some(&format!("service, in {waited_ms} ms")),
                    );
                }
                Err(e) => {
                    return Err(StepOutcome::failed(
                        format!(
                            "could not stop the previous gateway service: {e}; \
                             refusing to start a duplicate on :{port}"
                        ),
                        log.clone(),
                    ));
                }
            }
        }
        // 2. A gateway process: recorded in the pid file, else found
        // by the port it listens on. The pid file is NOT cleared by
        // the service branch above: after a real service stop its
        // pid (the service host's child) is dead and is skipped
        // here, while after a mode flip it may name a live
        // process-mode gateway that must still be stopped (SCM-7).
        // It is removed below, once nothing it names is running.
        let recorded = match ctx.ops.read_file(&pid_path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).trim().parse::<u32>().ok(),
            Err(_) => None,
        };
        if let Some(old_pid) = recorded {
            if ctx.ops.process_alive(old_pid) && ctx.ops.process_is_gateway(old_pid) == Some(false)
            {
                // The recorded pid now names another program (the old
                // gateway died, e.g. across a reboot, and Windows reused
                // the pid). `kill_process` is `taskkill /T /F`: killing it
                // would take down that program's whole tree (PL-M4).
                log.push(format!(
                    "recorded gateway pid {old_pid} now belongs to another program; \
                     treating the pid file as stale"
                ));
            }
        }
        let port_owner = ctx.ops.port_owner_pid(port);
        let found = crate::ops::pick_gateway_pid(
            recorded,
            port_owner,
            |p| ctx.ops.process_alive(p),
            |p| ctx.ops.process_is_gateway(p),
        );
        if let Some((old_pid, found_by)) = found {
            if found_by == crate::FoundBy::PortOwner {
                push_log(
                    ctx,
                    log,
                    format!(
                        "found a running gateway (pid {old_pid}) on :{port} that no pid file records"
                    ),
                );
            }
            gateway_mark(
                ctx,
                "stopping previous gateway",
                Some(&format!("pid {old_pid} (found by {})", found_by.label())),
            );
            if let Err(e) = ctx.ops.kill_process(old_pid) {
                return Err(StepOutcome::failed(
                    format!(
                        "could not stop the previous gateway (pid {old_pid}): {e}; \
                         refusing to spawn a duplicate on :{port}"
                    ),
                    log.clone(),
                ));
            }
            log.push(format!("stopped previous gateway (pid {old_pid})"));
            // Wait for the process to actually exit and release the
            // port before the replacement spawns.
            let gone = |ops: &dyn SystemOps| {
                !ops.process_alive(old_pid) && ops.port_owner_pid(port) != Some(old_pid)
            };
            for _ in 0..100 {
                if gone(ctx.ops.as_ref()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if !gone(ctx.ops.as_ref()) {
                return Err(StepOutcome::failed(
                    format!(
                        "previous gateway (pid {old_pid}) is still running after a stop \
                         request; refusing to spawn a duplicate on :{port}"
                    ),
                    log.clone(),
                ));
            }
            gateway_mark(
                ctx,
                "previous gateway stopped",
                Some(&format!("pid {old_pid}")),
            );
        }
        let _ = ctx.ops.remove_file(&pid_path).await;
        Ok(())
    }

    /// Service-mode start: regenerate the gateway service definition
    /// from the fully built spawn command and start the service,
    /// returning the gateway child's pid (recorded by the service
    /// host in the gateway pid file) for the health checks.
    pub(crate) async fn spawn_gateway_as_service(
        &self,
        ctx: &WizardContext,
        spawn: &PlannedCommand,
        log: &mut Vec<String>,
    ) -> Result<u32, StepOutcome> {
        let def = manager_services::gateway_definition(
            gateway_launch_spec(spawn, &ctx.config),
            self.gateway_stdout_log(ctx),
            self.gateway_stderr_log(ctx),
            crate::gateway_pid_path(&ctx.data_dir),
        );
        match ctx.ops.write_service_definition(&ctx.data_dir, &def).await {
            Ok(path) => log.push(format!(
                "wrote gateway service definition {}",
                path.display()
            )),
            Err(e) => {
                return Err(StepOutcome::failed(
                    format!("could not write the gateway service definition: {e}"),
                    log.clone(),
                ))
            }
        }
        ctx.ops.service_start_gateway(&def).await.map_err(|e| {
            StepOutcome::failed(format!("gateway service start failed: {e}"), log.clone())
        })
    }

    /// Files the gateway child's stdout/stderr are captured to.
    pub(crate) fn gateway_stdout_log(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("logs").join("gateway.stdout.log")
    }

    pub(crate) fn gateway_stderr_log(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("logs").join("gateway.stderr.log")
    }

    /// Append the tail of the captured gateway logs to the step log,
    /// so a startup failure shows the process's own error output in
    /// the wizard instead of only a probe timeout.
    pub(crate) async fn append_gateway_log_tail(&self, ctx: &WizardContext, log: &mut Vec<String>) {
        for (label, path) in [
            ("stderr", self.gateway_stderr_log(ctx)),
            ("stdout", self.gateway_stdout_log(ctx)),
        ] {
            if let Ok(bytes) = ctx.ops.read_file(&path).await {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                if !lines.is_empty() {
                    log.push(format!("gateway {label} ({}), last lines:", path.display()));
                    for l in lines.iter().rev().take(20).rev() {
                        log.push((*l).to_string());
                    }
                }
            }
        }
    }

    /// The directory holding the venv's executables (`Scripts\` on
    /// Windows, `bin/` elsewhere): the interpreter lives there, and
    /// so do the console scripts installed into the venv.
    pub(crate) fn venv_scripts_dir(&self, ctx: &WizardContext) -> PathBuf {
        self.venv_python(ctx)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.venv_dir(ctx))
    }

    /// PATH for a venv child process: the venv's script directory
    /// first, then the inherited PATH. The Prisma CLI spawns its
    /// Python generator, `prisma-client-py`, BY NAME during
    /// `prisma generate`; that executable lives in the venv's script
    /// directory, which is not on PATH just because the interpreter
    /// was invoked by absolute path — without the prepend, generate
    /// fails with "spawn prisma-client-py ENOENT" (found on real
    /// hardware 2026-10-16). Every command that drives the Prisma
    /// toolchain gets this PATH (see [`Self::with_prisma_env`]).
    pub(crate) fn venv_path_with_scripts(&self, ctx: &WizardContext) -> String {
        let scripts = self.venv_scripts_dir(ctx);
        let sep = if cfg!(windows) { ";" } else { ":" };
        match std::env::var_os("PATH") {
            Some(existing) if !existing.is_empty() => {
                format!("{}{sep}{}", scripts.display(), existing.to_string_lossy())
            }
            _ => scripts.display().to_string(),
        }
    }
}
