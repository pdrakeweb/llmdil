//! `StartGateway`: the install-mode run body and the service route.

use super::*;

impl StartGateway {
    /// The install-mode body of [`WizardStep::run`] (see
    /// [`Self::run_install_marked`] for the lifecycle markers around it).
    pub(crate) async fn run_install(
        &self,
        ctx: &WizardContext,
    ) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Built once, up front: every failure path below
        // attaches it, so a failed start always shows how
        // to start the gateway by hand.
        let manual = self.manual_start_block(ctx).await;
        let uv = match self.ensure_uv(ctx).await {
            Ok(p) => {
                push_log(ctx, &mut log, format!("using uv at {}", p.display()));
                p
            }
            Err(outcome) => return Ok(outcome.with_manual_command(manual)),
        };
        // Reuse a healthy existing venv: one already on the
        // pinned Python with litellm importable needs no venv
        // creation and no package install. A venv on the
        // pinned Python whose packages are missing or broken
        // needs only the package install — recreating the
        // venv (`uv venv --clear`) for a package-level
        // problem threw away a sound interpreter and redid
        // minutes of work on every retry. Only a venv on the
        // WRONG Python (or no venv at all) is (re)created;
        // --clear replaces a venv left behind by a failed
        // install or built with an unpinned (too new)
        // Python, which previously made every retry fail
        // with "already exists" before the real error.
        // The probes carry their failure evidence into the
        // log (see venv_python_version_probe): which repair
        // runs — and why — is always stated before it runs.
        let (existing_version, version_probe_detail) = self.venv_python_version_probe(ctx).await;
        let venv_current = existing_version.as_deref() == Some(crate::GATEWAY_PYTHON_VERSION);
        let (litellm_ok, import_probe_detail) = if venv_current {
            self.litellm_import_probe(ctx).await
        } else {
            (false, None)
        };
        // A healthy venv on a different LiteLLM version than
        // the pin (the operator moved the pin forward, or an
        // update rolled it back) gets a package-only install
        // of the pinned version: the pin is the version that
        // runs, never just the one installed first. An
        // undeterminable version is not treated as drift.
        let pinned_litellm = self.litellm_pinned_version(ctx);
        let probed_before_install = if litellm_ok {
            Some(self.installed_litellm_version(ctx).await)
        } else {
            None
        };
        let pin_drift = matches!(
            &probed_before_install,
            Some(Some(v)) if *v != pinned_litellm
        );
        let setup_cmds: Vec<PlannedCommand> = if pin_drift {
            push_log(
                ctx,
                &mut log,
                format!(
                    "venv has LiteLLM {} but the pin is {pinned_litellm} — installing the pinned version (the venv itself is kept)",
                    probed_before_install
                        .clone()
                        .flatten()
                        .unwrap_or_default()
                ),
            );
            vec![self.pip_command(ctx, &uv)]
        } else if litellm_ok {
            push_log(
                ctx,
                &mut log,
                format!(
                    "existing install verified — skipping install (venv on Python {}; litellm, prisma and prometheus_client import cleanly)",
                    crate::GATEWAY_PYTHON_VERSION
                ),
            );
            Vec::new()
        } else if venv_current {
            push_log(
                ctx,
                &mut log,
                format!(
                    "venv is on Python {} but the litellm import probe failed ({}) — installing packages only (the venv itself is kept)",
                    crate::GATEWAY_PYTHON_VERSION,
                    import_probe_detail.as_deref().unwrap_or("no detail")
                ),
            );
            vec![self.pip_command(ctx, &uv)]
        } else {
            match &existing_version {
                Some(v) => push_log(
                    ctx,
                    &mut log,
                    format!(
                        "existing venv uses Python {v}; recreating it with Python {}",
                        crate::GATEWAY_PYTHON_VERSION
                    ),
                ),
                None => {
                    if ctx.ops.file_exists(&self.venv_python(ctx)).await {
                        push_log(
                            ctx,
                            &mut log,
                            format!(
                                "the venv interpreter probe failed ({}) — recreating the venv with Python {}",
                                version_probe_detail.as_deref().unwrap_or("no detail"),
                                crate::GATEWAY_PYTHON_VERSION
                            ),
                        );
                    } else {
                        push_log(
                            ctx,
                            &mut log,
                            format!(
                                "creating venv with Python {}",
                                crate::GATEWAY_PYTHON_VERSION
                            ),
                        );
                    }
                }
            }
            vec![self.venv_command(ctx, &uv), self.pip_command(ctx, &uv)]
        };
        // Package installs get a longer command budget than
        // the ops default: a cold `uv pip install` of
        // litellm[proxy] + prisma on a slow link is the one
        // legitimate multi-minute command in the wizard.
        const SETUP_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
        // Route: service mode applies only when the gateway service is
        // actually registered — the same "mode AND registered" rule
        // the backends use (SCM-7). Otherwise the gateway runs as a
        // process, and the previous one is stopped by pid either way.
        let service_route = self.service_route(ctx, &mut log).await;
        // Restart semantics: the old gateway keeps serving through the
        // slow preparation (probes, MCP arming, YAML, PostgreSQL) and
        // is stopped just before the spawn — EXCEPT when the venv is
        // about to be repaired: recreating the venv or reinstalling
        // packages under a running interpreter fails on Windows (files
        // in use) and pulls the code out from under it, so the old
        // gateway is stopped first in that case.
        let mut previous_stopped = false;
        if !setup_cmds.is_empty() {
            push_log(
                ctx,
                &mut log,
                "stopping any running gateway before repairing its venv".to_string(),
            );
            if let Err(outcome) = self
                .stop_previous_gateway(ctx, service_route, &mut log)
                .await
            {
                return Ok(outcome.with_manual_command(manual));
            }
            previous_stopped = true;
        }
        // Hash enforcement (finding F1 follow-up): a covering lock is
        // written next to the venv and the install runs under
        // --require-hashes; any other pin installs by version only and
        // the outcome carries the "not hash-verified" warning.
        let mut unverified_install: Option<String> = None;
        if !setup_cmds.is_empty() {
            match self.hash_lock_mode(ctx) {
                Ok(()) => {
                    if let Err(e) = self.write_requirements_lock(ctx).await {
                        return Ok(StepOutcome::failed(e, log).with_manual_command(manual));
                    }
                    push_log(
                        ctx,
                        &mut log,
                        format!(
                            "installing from the hash lock {} (--require-hashes)",
                            self.requirements_lock_path(ctx).display()
                        ),
                    );
                }
                Err(warning) => {
                    push_log(ctx, &mut log, format!("warning: {warning}"));
                    unverified_install = Some(warning);
                }
            }
        }
        for cmd in &setup_cmds {
            match run_command_logged(ctx, &mut log, cmd, Some(SETUP_COMMAND_TIMEOUT)).await {
                Ok(out) if out.success() => {
                    if !out.stdout.trim().is_empty() {
                        log.push(out.stdout.trim().to_string());
                    }
                }
                Ok(out) => {
                    return Ok(StepOutcome::failed(
                        format!(
                            "command failed (exit {}): {}",
                            out.exit_code,
                            cmd.argv.join(" ")
                        ),
                        log,
                    )
                    .with_manual_command(manual))
                }
                Err(e) => {
                    return Ok(StepOutcome::failed(format!("{e}"), log).with_manual_command(manual))
                }
            }
        }
        // Probe the installed LiteLLM version before the
        // spawn (MCP design §4.3): the result is recorded in
        // the spawn state below and drives the MCP floor
        // check in the arming pass that follows. When nothing
        // was installed, the pre-install probe is the answer.
        let litellm_version = match probed_before_install {
            Some(v) if setup_cmds.is_empty() => v,
            _ => self.installed_litellm_version(ctx).await,
        };
        match &litellm_version {
            Some(v) => push_log(ctx, &mut log, format!("detected LiteLLM version: {v}")),
            None => push_log(
                ctx,
                &mut log,
                "LiteLLM version could not be determined (the version probe returned nothing usable)"
                    .to_string(),
            ),
        }
        // MCP gateway arming (design §4.2), resolved BEFORE
        // the YAML is generated: the generated config, the
        // spawn environment and the spawn fingerprint all key
        // off this one armed set (the single rule lives in
        // crate::armed_gateway_mcp_servers). Each Gateway
        // server's credential is read once here; a server
        // whose credential cannot be read is NOT armed — it
        // is omitted from this start with a warning and the
        // gateway start proceeds: a missing MCP credential
        // never fails the gateway.
        let mut mcp_cred_values: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for server in ctx.config.mcp.servers.iter().filter(|s| {
            s.exposure == manager_config::McpExposure::Gateway && !s.env_keys.is_empty()
        }) {
            let Some(cred_ref) = server.cred_ref.as_deref().filter(|r| !r.is_empty()) else {
                log.push(format!(
                    "warning: MCP server '{}' is not armed: it declares env keys but no credential reference — the server is omitted from this gateway start",
                    server.id
                ));
                continue;
            };
            if mcp_cred_values.contains_key(cred_ref) {
                continue;
            }
            match ctx
                .ops
                .credential_read(cred_ref)
                .await
                .filter(|v| !v.is_empty())
            {
                Some(value) => {
                    mcp_cred_values.insert(cred_ref.to_string(), value);
                }
                None => log.push(format!(
                    "warning: MCP server '{}' is not armed: credential '{cred_ref}' could not be read from Windows Credential Manager — the server is omitted from this gateway start",
                    server.id
                )),
            }
        }
        let mcp_armed: Vec<&manager_config::McpServerConfig> =
            crate::armed_gateway_mcp_servers(&ctx.config, |t| mcp_cred_values.contains_key(t));
        let mcp_stdio_armed = crate::mcp_armed_set_has_stdio(&mcp_armed);
        // Floor enforcement (design §4.3), fail-closed BEFORE
        // any spawn: with an armed stdio Gateway server, the
        // installed LiteLLM must be at or above
        // LITELLM_MCP_FLOOR_VERSION (the release carrying the
        // stdio command/args allowlist fix). An UNDETERMINED
        // version also fails: the allowlist state would be
        // unverifiable, and the CVE surface is exactly that
        // allowlist — a deliberate conservative reading.
        if mcp_stdio_armed {
            let meets_floor = litellm_version
                .as_deref()
                .and_then(crate::litellm_version_meets_mcp_floor);
            if meets_floor != Some(true) {
                return Ok(StepOutcome::failed(
                    format!(
                        "MCP stdio gateway servers require LiteLLM >= {} (the stdio allowlist fix), but the installed version is {}. Remediation: set updates.components[\"litellm\"].pinned_version to a version at or above {} (or clear a stale pin) and re-run this step",
                        crate::LITELLM_MCP_FLOOR_VERSION,
                        litellm_version
                            .as_deref()
                            .unwrap_or("unknown — the version probe could not determine it"),
                        crate::LITELLM_MCP_FLOOR_VERSION
                    ),
                    log,
                )
                .with_manual_command(manual));
            }
        }
        // config.yaml is generated from app state, never hand-edited.
        // Both failure paths carry the manual block and the
        // accumulated step log, like every other failure in
        // this step — they previously returned a one-line
        // debug outcome with neither (code-review 2.8).
        // Generate from the armed-only view of the config:
        // unarmed Gateway servers are omitted from this
        // start's YAML (the generator itself stays a pure
        // function of the config it is handed).
        let generation_config = crate::config_with_armed_gateway_mcp_servers(&ctx.config, |t| {
            mcp_cred_values.contains_key(t)
        });
        let yaml = match manager_gateway::generate_config_yaml(&generation_config) {
            Ok(y) => y,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("gateway config generation failed: {e}"),
                    log,
                )
                .with_manual_command(manual))
            }
        };
        let cfg_path = ctx.data_dir.join("gateway").join("config.yaml");
        // The applied stamp written on success carries this instant:
        // activation-journal entries recorded after it (during this
        // very run) stay pending (CR-1).
        let yaml_written_at = chrono::Utc::now();
        if let Err(e) = ctx.ops.write_file(&cfg_path, yaml.as_bytes()).await {
            return Ok(StepOutcome::failed(
                format!(
                    "could not write the gateway configuration {}: {e}",
                    cfg_path.display()
                ),
                log,
            )
            .with_manual_command(manual));
        }
        log.push(format!("wrote {}", cfg_path.display()));
        // The context-overflow routing callback
        // (routing-rules design §6.3) ships as a generated
        // sibling the config's `callbacks:` entry resolves
        // config-directory-relative. Written when a typed
        // rule is enabled; a stale copy is removed when not,
        // so disabling the rule restores exactly the
        // pre-feature gateway directory.
        let routing_path = ctx.data_dir.join("gateway").join("llmdil_routing.py");
        match manager_gateway::generate_routing_callback(&generation_config) {
            Some(callback_py) => {
                if let Err(e) = ctx
                    .ops
                    .write_file(&routing_path, callback_py.as_bytes())
                    .await
                {
                    return Ok(StepOutcome::failed(
                        format!(
                            "could not write the gateway routing callback {}: {e}",
                            routing_path.display()
                        ),
                        log,
                    )
                    .with_manual_command(manual));
                }
                log.push(format!("wrote {}", routing_path.display()));
            }
            None => {
                if let Err(e) = ctx.ops.remove_file(&routing_path).await {
                    return Ok(StepOutcome::failed(
                        format!(
                            "could not remove the stale gateway routing callback {}: {e}",
                            routing_path.display()
                        ),
                        log,
                    )
                    .with_manual_command(manual));
                }
            }
        }
        // App-managed PostgreSQL: when install-postgres has
        // run, the server must be up before the gateway
        // spawns — the gateway receives DATABASE_URL and
        // migrates against it at startup. Not installed →
        // no DATABASE_URL, the pre-PostgreSQL behavior.
        // Ensured BEFORE the previous gateway is stopped: a
        // running server is simply adopted (a gateway restart
        // never restarts PostgreSQL unless its port changed),
        // and a PostgreSQL failure leaves the old gateway
        // serving instead of taking it down for nothing.
        let postgres_url = if postgres_installed(ctx.ops.as_ref(), &ctx.data_dir).await {
            match ensure_postgres_running_detailed(ctx, &mut log).await {
                Ok((url, how)) => {
                    gateway_mark(ctx, "PostgreSQL ready", Some(&how.describe()));
                    Some(url)
                }
                Err(e) => return Ok(StepOutcome::failed(e, log).with_manual_command(manual)),
            }
        } else {
            None
        };
        // Restart = stop the previous gateway now, just before the
        // spawn (unless the venv repair above already did); without
        // this the spawn below is a duplicate that fails to bind
        // while the old process keeps serving.
        if !previous_stopped {
            if let Err(outcome) = self
                .stop_previous_gateway(ctx, service_route, &mut log)
                .await
            {
                return Ok(outcome.with_manual_command(manual));
            }
        }
        let database_url = match postgres_url {
            Some(url) => {
                // Verify (and only if needed, repair)
                // the Prisma install before the spawn:
                // the gateway imports the Prisma client
                // at startup and dies if the install is
                // broken, so that work happens here,
                // where its failure is reported as this
                // step's failure — not as a dead gateway.
                // On a healthy install this is three
                // checks and no mutation (see
                // ensure_prisma_client_generated); the
                // old `--version` pre-warm is gone — it
                // generated nothing, proved nothing the
                // connect probe does not prove better,
                // and cost a process spawn on every
                // start.
                if let Err(outcome) = self
                    .ensure_prisma_client_generated(ctx, &uv, &url, &mut log)
                    .await
                {
                    return Ok(outcome.with_manual_command(manual));
                }
                Some(url)
            }
            None => None,
        };
        // The spawn env's MCP entries: each armed server's
        // credential value (read once in the arming pass)
        // under each of its env-key names.
        let mcp_env: Vec<(String, String)> = mcp_armed
            .iter()
            .flat_map(|s| {
                let value = s
                    .cred_ref
                    .as_deref()
                    .and_then(|r| mcp_cred_values.get(r))
                    .cloned();
                s.env_keys
                    .iter()
                    .filter_map(move |k| value.clone().map(|v| (k.clone(), v)))
            })
            .collect();
        let (mut spawn, openrouter_warning) = match self
            .spawn_command_with_env(ctx, database_url.as_deref(), &mcp_env, mcp_stdio_armed)
            .await
        {
            Ok(pair) => pair,
            Err(outcome) => return Ok(outcome.with_manual_command(manual)),
        };
        if let Some(w) = &openrouter_warning {
            log.push(format!("warning: {w}"));
        }
        // Capture the gateway's own output: without it a
        // startup failure is undiagnosable from inside the app.
        spawn.stdout_log = Some(self.gateway_stdout_log(ctx));
        spawn.stderr_log = Some(self.gateway_stderr_log(ctx));
        push_log(
            ctx,
            &mut log,
            format!(
                "gateway output is captured to {} and {}",
                self.gateway_stdout_log(ctx).display(),
                self.gateway_stderr_log(ctx).display()
            ),
        );
        push_log(
            ctx,
            &mut log,
            format!(
                "spawning the gateway process ({})",
                spawn.argv.first().cloned().unwrap_or_default()
            ),
        );
        let spawned_at = std::time::Instant::now();
        let pid = if service_route {
            // Service mode: regenerate the gateway's service
            // definition from this spawn (secret values become
            // credential references — never written to the
            // definition file) and start the service. The
            // health checks below run unchanged against the
            // child pid the service host recorded.
            match self.spawn_gateway_as_service(ctx, &spawn, &mut log).await {
                Ok(pid) => pid,
                Err(outcome) => return Ok(outcome.with_manual_command(manual)),
            }
        } else {
            match ctx.ops.spawn_detached(&spawn).await {
                Ok(pid) => pid,
                Err(e) => {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])
                        .with_manual_command(manual))
                }
            }
        };
        push_log(ctx, &mut log, format!("LiteLLM spawned (pid {pid})"));
        gateway_mark(
            ctx,
            "spawned",
            Some(&format!(
                "pid {pid} ({})",
                if service_route { "service" } else { "process" }
            )),
        );
        // Record the pid so a later stop (the gateway_stop
        // command) or restart (this step re-run) terminates
        // exactly this process instead of guessing by port.
        let pid_path = crate::gateway_pid_path(&ctx.data_dir);
        match ctx
            .ops
            .write_file(&pid_path, pid.to_string().as_bytes())
            .await
        {
            Ok(()) => push_log(
                ctx,
                &mut log,
                format!("recorded gateway pid in {}", pid_path.display()),
            ),
            Err(e) => log.push(format!(
                "warning: could not record the gateway pid in {}: {e}; \
                 stopping this gateway later will not be possible",
                pid_path.display()
            )),
        }
        // First-healthy is not enough on the database path:
        // the known database defects kill the gateway about
        // a second after it first answers, so that path also
        // has to survive the confirmation window.
        let health_failure = if !self.wait_healthy(ctx, pid, &mut log).await {
            Some("did not become healthy")
        } else if let Some(url) = &database_url {
            if self.confirm_sustained_health(ctx, pid, url, &mut log).await {
                None
            } else {
                Some("became healthy but did not stay up")
            }
        } else {
            None
        };
        if health_failure.is_none() {
            gateway_mark(
                ctx,
                "healthy",
                Some(&format!(
                    "pid {pid} after {:.1} s",
                    spawned_at.elapsed().as_secs_f64()
                )),
            );
        }
        if let Some(health_failure) = health_failure {
            // The gateway's own output is the diagnosis —
            // appended to the step log above its tail,
            // LABELED as gateway output. The failure MESSAGE
            // is the step's own verdict only: embedding the
            // raw stderr tail in the message presented it as
            // the step's error, and unrelated gateway log
            // noise (auth rejections from the app's own
            // status polling) reached the operator labeled
            // as the cause of the failure (2026-10-16).
            self.append_gateway_log_tail(ctx, &mut log).await;
            return Ok(StepOutcome::failed(
                format!(
                    "gateway {health_failure} on :{} — the gateway's output is in the step log and in {}",
                    ctx.config.gateway.port,
                    ctx.data_dir.join("logs").display()
                ),
                log,
            )
            .with_manual_command(manual));
        }
        // Post-start MCP load check (design §8, §11, §14.6):
        // every armed server must appear in the gateway's
        // registered list and answer its tools list. A
        // failure is logged with the gateway's own error
        // text and noted in the outcome message, but never
        // fails the step — the gateway itself is healthy.
        // Nothing is persisted: the MCP view re-queries the
        // gateway live (Phase 4).
        let mcp_load_failed: Vec<String> = if mcp_armed.is_empty() {
            Vec::new()
        } else {
            let armed_ids: Vec<String> = mcp_armed.iter().map(|s| s.id.clone()).collect();
            self.mcp_load_check(ctx, &armed_ids, &mut log).await
        };
        // Record what this gateway was actually spawned
        // with (config fingerprint + the YAML): the drift
        // check at the next launch compares the record
        // against the then-current configuration, YAML and
        // spawn-only inputs alike (code-review 2.3).
        // The port the previous gateway listened on, before this start's
        // record replaces it: its firewall rule is closed below.
        let previous_port;
        {
            let fingerprint = crate::gateway_spawn_fingerprint(
                &ctx.config,
                database_url.is_some(),
                spawn.env.get("LITELLM_MASTER_KEY").map(String::as_str),
                spawn.env.get("OPENROUTER_API_KEY").map(String::as_str),
                &mcp_armed,
            );
            let state_json =
                crate::gateway_spawn_state_json(&fingerprint, &yaml, litellm_version.as_deref());
            previous_port = ctx
                .ops
                .read_file(&crate::gateway_spawn_state_path(&ctx.data_dir))
                .await
                .ok()
                .and_then(|b| serde_json::from_slice::<crate::GatewaySpawnState>(&b).ok())
                .map(|s| s.fingerprint.port);
            match ctx
                .ops
                .write_file(
                    &crate::gateway_spawn_state_path(&ctx.data_dir),
                    state_json.as_bytes(),
                )
                .await
            {
                Ok(()) => log.push(
                    "recorded the gateway spawn state for the next launch's drift check"
                        .to_string(),
                ),
                Err(e) => log.push(format!(
                    "warning: could not record the gateway spawn state: {e}; \
                     the next launch will treat this start as drifted and re-run this step"
                )),
            }
        }
        let mut message = match &openrouter_warning {
            Some(w) => format!(
                "LiteLLM gateway running on :{} — warning: {w}",
                ctx.config.gateway.port
            ),
            None => format!("LiteLLM gateway running on :{}", ctx.config.gateway.port),
        };
        // The gateway is healthy; reconcile the Windows
        // firewall rule with the bind address so LAN clients
        // can actually reach a LAN-bound gateway.
        if let Some(fw) = self
            .ensure_gateway_firewall(ctx, previous_port, &mut log)
            .await
        {
            log.push(format!("warning: {fw}"));
            message = format!("{message} — warning: {fw}");
        }
        if let Some(w) = &unverified_install {
            message = format!("{message} — warning: {w}");
        }
        if !mcp_load_failed.is_empty() {
            message = format!(
                "{message} — warning: MCP load check failed for: {}",
                mcp_load_failed.join(", ")
            );
        }
        // Final re-check, immediately before reporting
        // done: between the health confirmation above and
        // here, the step runs the MCP load check and the
        // firewall reconcile — the reconcile can wait on a
        // UAC prompt for minutes — and a gateway that died
        // in that window was still reported as a successful
        // start (field report 2026-10-19: the step said
        // "done" while the Gateway page showed the gateway
        // down). One last probe at the health check's own
        // standard: the spawned pid must still be alive — in
        // service mode too: the service host does not restart
        // the gateway child, so a dead pid is a dead gateway, and
        // a probe answering from some OTHER process on the port
        // (an old gateway left over by a mode flip) must not pass
        // for this start (SCM-7) — in service mode the SCM must
        // also still report the service Running; and the
        // authenticated models endpoint must answer 200. A
        // failure here fails the step with the gateway's output
        // attached, like any other health failure.
        let scm_state = if service_route {
            ctx.ops
                .service_state(manager_services::GATEWAY_SERVICE_NAME)
                .await
        } else {
            None
        };
        let final_check_failure = if !ctx.ops.process_alive(pid) {
            Some(format!(
                "the gateway process (pid {pid}) exited after its health checks passed"
            ))
        } else if let Some(state) =
            scm_state.filter(|s| *s != manager_services::ServiceState::Running)
        {
            Some(format!(
                "the gateway service is {} (not running) after its health checks passed",
                state.as_str()
            ))
        } else {
            let headers = self.gateway_auth_headers(ctx).await;
            match ctx
                    .ops
                    .http_get_with_headers(&self.gateway_url(ctx), &headers)
                    .await
                {
                    Ok(resp) if resp.status == 200 => None,
                    Ok(resp) => Some(format!(
                        "the gateway answered HTTP {} on the final health re-check after passing its health checks",
                        resp.status
                    )),
                    Err(e) => Some(format!(
                        "the gateway is unreachable on the final health re-check ({e}) after passing its health checks"
                    )),
                }
        };
        if let Some(reason) = final_check_failure {
            self.append_gateway_log_tail(ctx, &mut log).await;
            return Ok(StepOutcome::failed(
                format!(
                    "gateway started but did not stay up: {reason} — the gateway's output is in the step log and in {}",
                    ctx.data_dir.join("logs").display()
                ),
                log,
            )
            .with_manual_command(manual));
        }
        push_log(
            ctx,
            &mut log,
            "final health re-check passed — the gateway is still serving".to_string(),
        );
        // Only now is the new configuration live: stamp it, so the
        // activation plan resolves gateway-scope entries from a
        // successful start and never from the YAML write alone
        // (CR-1). Written through the ops layer (hermetic tests).
        let stamp_path = crate::gateway_applied_stamp_path(&ctx.data_dir);
        match ctx
            .ops
            .write_file(
                &stamp_path,
                crate::gateway_applied_stamp_json(yaml_written_at, pid).as_bytes(),
            )
            .await
        {
            Ok(()) => log.push(format!("recorded the applied stamp in {}", stamp_path.display())),
            Err(e) => log.push(format!(
                "warning: could not record the applied stamp in {}: {e};                  the activation plan may keep showing these changes as pending",
                stamp_path.display()
            )),
        }
        gateway_mark(
            ctx,
            "started",
            Some(&format!(
                "pid {pid} serving on :{}",
                ctx.config.gateway.port
            )),
        );
        Ok(StepOutcome::done(message, log))
    }

    /// [`Self::run_install`] wrapped in the gateway's lifecycle
    /// markers (`logs/gateway.lifecycle.log`): one when the step
    /// starts and, on any failure return, one carrying the reason —
    /// so the gateway's own log scope shows every start/restart
    /// attempt and how it ended, not only `wizard.log` (GW-4).
    pub(crate) async fn run_install_marked(
        &self,
        ctx: &WizardContext,
    ) -> Result<StepOutcome, WizardError> {
        gateway_mark(ctx, "start-gateway step started", None);
        let result = self.run_install(ctx).await;
        match &result {
            Ok(out) if out.state == crate::StepState::Failed => {
                gateway_mark(ctx, "start failed", Some(first_line(&out.message)))
            }
            Err(e) => gateway_mark(ctx, "start failed", Some(first_line(&e.to_string()))),
            Ok(_) => {}
        }
        result
    }

    /// The gateway's route for this run: the SCM service when
    /// service supervision is on AND the gateway service is
    /// registered; the process path otherwise (a service-mode
    /// install whose gateway service is not registered falls back
    /// to process supervision, as backends do — SCM-7).
    pub(crate) async fn service_route(&self, ctx: &WizardContext, log: &mut Vec<String>) -> bool {
        if ctx.config.supervision.mode != SupervisionMode::Service {
            return false;
        }
        if ctx
            .ops
            .service_registered(manager_services::GATEWAY_SERVICE_NAME)
            .await
        {
            return true;
        }
        push_log(
            ctx,
            log,
            format!(
                "service supervision is on but the gateway service '{}' is not registered — running the gateway as a process",
                manager_services::GATEWAY_SERVICE_NAME
            ),
        );
        false
    }
}
