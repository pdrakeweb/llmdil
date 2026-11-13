//! `StartGateway`: Prisma client pin, generation, and repair.

use super::*;

impl StartGateway {
    /// Attach the environment the Prisma commands run with: the
    /// gateway's DATABASE_URL (the LiteLLM schema's datasource reads
    /// it from the environment) and the PATH from
    /// [`Self::venv_path_with_scripts`].
    pub(crate) fn with_prisma_env(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        mut cmd: PlannedCommand,
    ) -> PlannedCommand {
        cmd.env
            .insert("DATABASE_URL".to_string(), database_url.to_string());
        cmd.env
            .insert("PATH".to_string(), self.venv_path_with_scripts(ctx));
        cmd
    }

    /// Probe that succeeds only when the venv's Prisma client is
    /// generated AND functional standalone: it instantiates the
    /// client and connects it against the real DATABASE_URL. A
    /// plain import is weaker evidence than the gateway needs —
    /// the connect exercises the generated client, the engine, and
    /// the database in one pass. Until `prisma generate` has run
    /// at all, the import inside this probe raises "The Client
    /// hasn't been generated yet" and the probe fails the same way
    /// the gateway would. Scope limit: this probe cannot see
    /// defects inside LiteLLM's own Prisma wrapper (the
    /// '_Prisma__engine' failure tracked in
    /// docs/debugging/gateway-start-troubleshooting.md passes a
    /// standalone connect) — that class is closed by the version
    /// pin enforced in [`Self::ensure_prisma_pin`], not by
    /// detection here.
    pub(crate) fn prisma_client_probe_command(
        &self,
        ctx: &WizardContext,
        database_url: &str,
    ) -> PlannedCommand {
        const SCRIPT: &str = concat!(
            "import asyncio\n",
            "from prisma import Prisma\n",
            "\n",
            "async def _probe():\n",
            "    client = Prisma()\n",
            "    await client.connect()\n",
            "    await client.disconnect()\n",
            "\n",
            "asyncio.run(_probe())\n",
        );
        self.with_prisma_env(
            ctx,
            database_url,
            PlannedCommand::new(vec![
                self.venv_python(ctx).display().to_string(),
                "-c".to_string(),
                SCRIPT.to_string(),
            ]),
        )
    }

    /// Run the connect probe ([`Self::prisma_client_probe_command`]):
    /// None when the client is healthy, otherwise a one-line
    /// description of the failure for the step log and, at the end
    /// of the recovery sequence, the step's failure message.
    pub(crate) async fn prisma_probe_failure(
        &self,
        ctx: &WizardContext,
        database_url: &str,
    ) -> Option<String> {
        let probe = self.prisma_client_probe_command(ctx, database_url);
        // A healthy connect answers in well under a second against
        // the PostgreSQL the step just started; the budget only
        // bounds a wedged engine startup.
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        match ctx
            .ops
            .run_command_with_timeout(&probe, PROBE_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => None,
            Ok(out) => {
                let detail = if out.stderr.trim().is_empty() {
                    out.stdout.trim().to_string()
                } else {
                    out.stderr.trim().to_string()
                };
                Some(format!("exit {}: {}", out.exit_code, detail))
            }
            Err(e) => Some(format!("{e}")),
        }
    }

    /// Command printing the path of the schema the installed
    /// LiteLLM ships (`<litellm>/proxy/schema.prisma`), resolved
    /// from the package itself rather than a guessed layout.
    fn prisma_schema_command(&self, ctx: &WizardContext, database_url: &str) -> PlannedCommand {
        self.with_prisma_env(
            ctx,
            database_url,
            PlannedCommand::new(vec![
                self.venv_python(ctx).display().to_string(),
                "-c".to_string(),
                "import litellm, pathlib; print(pathlib.Path(litellm.__file__).parent / 'proxy' / 'schema.prisma')"
                    .to_string(),
            ]),
        )
    }

    /// `prisma generate` against the LiteLLM schema.
    fn prisma_generate_command(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        schema: &Path,
    ) -> PlannedCommand {
        self.with_prisma_env(
            ctx,
            database_url,
            PlannedCommand::new(vec![
                self.venv_python(ctx).display().to_string(),
                "-m".to_string(),
                "prisma".to_string(),
                "generate".to_string(),
                "--schema".to_string(),
                schema.display().to_string(),
            ]),
        )
    }

    /// Resolve the LiteLLM schema path inside the venv, failing the
    /// step (with a clear message) when it cannot be located: a
    /// gateway started without generation would die at startup, so
    /// there is no useful fallback from here.
    async fn resolve_prisma_schema(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        log: &[String],
    ) -> Result<PathBuf, StepOutcome> {
        let cmd = self.prisma_schema_command(ctx, database_url);
        let out = match ctx.ops.run_command(&cmd).await {
            Ok(out) if out.success() => out,
            Ok(out) => {
                return Err(StepOutcome::failed(
                    format!(
                        "could not locate the LiteLLM Prisma schema (exit {}): {}",
                        out.exit_code,
                        out.stderr.trim()
                    ),
                    log.to_vec(),
                ))
            }
            Err(e) => {
                return Err(StepOutcome::failed(
                    format!("could not locate the LiteLLM Prisma schema: {e}"),
                    log.to_vec(),
                ))
            }
        };
        let raw = out.stdout.trim();
        if raw.is_empty() {
            return Err(StepOutcome::failed(
                "could not locate the LiteLLM Prisma schema: the venv's litellm package did not report a schema path",
                log.to_vec(),
            ));
        }
        let path = PathBuf::from(raw);
        if !ctx.ops.file_exists(&path).await {
            return Err(StepOutcome::failed(
                format!(
                    "the LiteLLM Prisma schema was not found at {} — the litellm installation in the venv is incomplete; re-running this step reinstalls it",
                    path.display()
                ),
                log.to_vec(),
            ));
        }
        Ok(path)
    }

    /// Run `prisma generate` against the schema, logging the
    /// stage; failure is the step's failure. First generation
    /// downloads the Prisma engines and a Node runtime, so it gets
    /// the same budget as a cold package install.
    async fn run_prisma_generate(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        schema: &Path,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let generate = self.prisma_generate_command(ctx, database_url, schema);
        const GENERATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
        match run_command_logged(ctx, log, &generate, Some(GENERATE_TIMEOUT)).await {
            Ok(out) if out.success() => {
                if !out.stdout.trim().is_empty() {
                    log.push(out.stdout.trim().to_string());
                }
                log.push("Prisma client generated".to_string());
                Ok(())
            }
            Ok(out) => {
                let output = if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                };
                Err(StepOutcome::failed(
                    format!(
                        "prisma generate failed (exit {}): {}",
                        out.exit_code, output
                    ),
                    log.clone(),
                ))
            }
            Err(e) => Err(StepOutcome::failed(
                format!("prisma generate could not run: {e}"),
                log.clone(),
            )),
        }
    }

    /// Probe printing the installed prisma-client-py version, or
    /// nothing usable when the package is absent or broken.
    pub(crate) fn prisma_version_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import importlib.metadata; print(importlib.metadata.version('prisma'))".to_string(),
        ])
    }

    /// The prisma version probe WITH its evidence (see
    /// [`Self::venv_python_version_probe`]): on failure the second
    /// element says why, so a repair decision can be logged with
    /// its cause. The parse tolerates noise around the version
    /// line ([`parse_semver_version`]) — output pollution used to
    /// read as "version unknown" and trigger the full pin repair
    /// (package removal + reinstall + regenerate) on every start.
    async fn installed_prisma_version_probe(
        &self,
        ctx: &WizardContext,
    ) -> (Option<String>, Option<String>) {
        match ctx
            .ops
            .run_command_with_timeout(&self.prisma_version_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => match parse_semver_version(&out.stdout) {
                Some(v) => (Some(v), None),
                None => (
                    None,
                    Some(format!(
                        "no version in the probe output: {}",
                        output_tail_lines(out.stdout.trim(), 2).join(" | ")
                    )),
                ),
            },
            Ok(out) => (None, Some(probe_failure_detail(&out))),
            Err(e) => (None, Some(format!("the probe could not run: {e}"))),
        }
    }

    /// Probe printing the installed LiteLLM version, or nothing
    /// usable when the package is absent or broken. Mirrors
    /// [`Self::prisma_version_command`] (MCP design §4.3).
    pub(crate) fn litellm_version_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import importlib.metadata; print(importlib.metadata.version('litellm'))".to_string(),
        ])
    }

    /// The venv's installed LiteLLM version ("1.105.1"), or None
    /// when it cannot be determined (package missing, metadata
    /// broken) — callers record None as "not measured" and never
    /// guess. Phase 0 records the probed version in the spawn
    /// state; the MCP floor check against
    /// [`crate::LITELLM_MCP_FLOOR_VERSION`] is enforced from
    /// Phase 2, when gateway-registered stdio servers exist.
    pub(crate) async fn installed_litellm_version(&self, ctx: &WizardContext) -> Option<String> {
        match ctx
            .ops
            .run_command_with_timeout(&self.litellm_version_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => parse_semver_version(&out.stdout),
            _ => None,
        }
    }

    /// The `prisma` package directory inside the venv's
    /// site-packages: where the library AND the generated client
    /// live (`Lib\site-packages` on Windows, `lib/pythonX.Y/
    /// site-packages` elsewhere, matching the pinned interpreter).
    pub(crate) fn prisma_package_dir(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(target_os = "windows") {
            venv.join("Lib").join("site-packages").join("prisma")
        } else {
            venv.join("lib")
                .join(format!("python{}", crate::GATEWAY_PYTHON_VERSION))
                .join("site-packages")
                .join("prisma")
        }
    }

    /// Reinstall exactly the pinned prisma-client-py into the venv.
    /// `--reinstall-package` forces replacement even when the
    /// resolver considers some prisma already installed (its
    /// dist-info survives the package-directory removal in
    /// [`Self::ensure_prisma_pin`]).
    fn prisma_pin_command(&self, ctx: &WizardContext, uv: &Path) -> PlannedCommand {
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            self.venv_python(ctx).display().to_string(),
            "--reinstall-package".to_string(),
            "prisma".to_string(),
            format!("prisma=={}", crate::PRISMA_PINNED_VERSION),
        ])
    }

    /// Bring the venv's prisma-client-py to the pinned version
    /// ([`crate::PRISMA_PINNED_VERSION`]) before any generation:
    /// LiteLLM's PrismaWrapper is incompatible with prisma >= 0.13,
    /// and the standalone connect probe cannot detect that (the
    /// wrapper failure fires inside LiteLLM, after a standalone
    /// connect succeeds), so the version itself is the check.
    ///
    /// On a mismatch the generated client is cleared FIRST: the
    /// files `prisma generate` writes into the package directory
    /// are not tracked by the installer, so they survive reinstalls
    /// and version changes, and the stale client then crashes
    /// imports with errors from the wrong version (field evidence
    /// 2026-10-16: a downgrade left the 0.15-generated client in
    /// place and importing prisma died with a misleading
    /// `AttributeError: module 'prisma._types' has no attribute
    /// 'SortMode'`). Removing the whole package directory and
    /// reinstalling the pinned package leaves a clean wheel state;
    /// the caller's generate step then produces a fresh client.
    async fn ensure_prisma_pin(
        &self,
        ctx: &WizardContext,
        uv: &Path,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let (installed, probe_detail) = self.installed_prisma_version_probe(ctx).await;
        if installed.as_deref() == Some(crate::PRISMA_PINNED_VERSION) {
            return Ok(());
        }
        push_log(
            ctx,
            log,
            format!(
                "installed prisma-client-py ({}) does not match the pinned {} — clearing the stale generated client and reinstalling the pinned package",
                match (&installed, &probe_detail) {
                    (Some(v), _) => v.clone(),
                    (None, Some(detail)) => format!("unknown — probe failed: {detail}"),
                    (None, None) => "unknown".to_string(),
                },
                crate::PRISMA_PINNED_VERSION
            ),
        );
        let dir = self.prisma_package_dir(ctx);
        if ctx.ops.file_exists(&dir).await {
            ctx.ops.remove_dir_all(&dir).await.map_err(|e| {
                StepOutcome::failed(
                    format!(
                        "could not remove the stale Prisma package directory {}: {e}",
                        dir.display()
                    ),
                    log.clone(),
                )
            })?;
        }
        let cmd = self.prisma_pin_command(ctx, uv);
        const PIN_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
        match run_command_logged(ctx, log, &cmd, Some(PIN_INSTALL_TIMEOUT)).await {
            Ok(out) if out.success() => {
                push_log(
                    ctx,
                    log,
                    format!(
                        "prisma-client-py {} installed",
                        crate::PRISMA_PINNED_VERSION
                    ),
                );
                Ok(())
            }
            Ok(out) => {
                let output = if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                };
                Err(StepOutcome::failed(
                    format!(
                        "installing the pinned prisma-client-py {} failed (exit {}): {}",
                        crate::PRISMA_PINNED_VERSION,
                        out.exit_code,
                        output
                    ),
                    log.clone(),
                ))
            }
            Err(e) => Err(StepOutcome::failed(
                format!(
                    "installing the pinned prisma-client-py {} could not run: {e}",
                    crate::PRISMA_PINNED_VERSION
                ),
                log.clone(),
            )),
        }
    }

    /// The generated client's entry point inside the prisma
    /// package: `prisma generate` writes `client.py` into the
    /// package directory, and the bare wheel does not contain one —
    /// its absence is exactly what makes the package raise "The
    /// Client hasn't been generated yet" (verified against the
    /// pinned 0.11.0 wheel: `prisma/__init__.py` imports
    /// `.client`, which only generation creates). Presence of this
    /// file is therefore the generation check — no process spawn
    /// needed.
    pub(crate) fn generated_client_path(&self, ctx: &WizardContext) -> PathBuf {
        self.prisma_package_dir(ctx).join("client.py")
    }

    /// Durable record of an in-progress Prisma repair:
    /// `<data_dir>/gateway/prisma-repair.json`. Written BEFORE the
    /// repair's first mutation and updated per phase; removed only
    /// when the repair verifies complete. Its presence at the start
    /// of a run means the previous repair was interrupted (app
    /// closed, step timeout) and is being resumed — the failure
    /// mode behind the 2026-10-16 half-repaired venv: the pin
    /// reinstall had deleted the generated client, the generate
    /// never ran, and nothing on disk said so.
    pub(crate) fn prisma_repair_state_path(ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("gateway").join("prisma-repair.json")
    }

    async fn write_prisma_repair_state(
        &self,
        ctx: &WizardContext,
        phase: &str,
        log: &mut Vec<String>,
    ) {
        let body = serde_json::json!({
            "pin": crate::PRISMA_PINNED_VERSION,
            "phase": phase,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        if let Err(e) = ctx
            .ops
            .write_file(
                &Self::prisma_repair_state_path(ctx),
                json_pretty(&body).as_bytes(),
            )
            .await
        {
            // Best-effort: the repair itself still runs; only the
            // resume bookkeeping is lost.
            log.push(format!(
                "warning: could not record the Prisma repair state ({phase}): {e}"
            ));
        }
    }

    /// Ensure the venv's Prisma client is generated AND connects
    /// before the gateway spawns with DATABASE_URL. LiteLLM imports
    /// `prisma.Prisma` while starting; with no generated client the
    /// import raises RuntimeError ("The Client hasn't been
    /// generated yet") and the process dies in its lifespan — the
    /// exact failure this closes (found on real hardware
    /// 2026-10-16: the step's only Prisma touch was a `--version`
    /// pre-warm, which generates nothing). Without a database the
    /// gateway never imports Prisma, so this runs only on the
    /// DATABASE_URL path.
    ///
    /// VERIFY-FIRST: the overwhelmingly common start — the install
    /// was healthy last time — must cost seconds and mutate
    /// nothing. Verification is three checks: the installed version
    /// equals the pin (package metadata, no reinstall), the
    /// generated client is present ([`Self::generated_client_path`]),
    /// and the standalone connect probe passes. Only when a check
    /// fails does the repair machinery run: pin reinstall
    /// ([`Self::ensure_prisma_pin`]) → probe → generate → re-probe,
    /// bracketed by the durable repair state
    /// ([`Self::prisma_repair_state_path`]) so an interrupted repair
    /// is resumed by the next run instead of inherited half-done.
    /// A client that still does not connect standalone after a
    /// successful generate fails the step with the probe output
    /// instead of spawning a gateway that cannot use its database.
    pub(crate) async fn ensure_prisma_client_generated(
        &self,
        ctx: &WizardContext,
        uv: &Path,
        database_url: &str,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        if ctx
            .ops
            .file_exists(&Self::prisma_repair_state_path(ctx))
            .await
        {
            log.push(
                "a previous Prisma repair did not complete (the app closed or the step timed out mid-repair) — resuming it"
                    .to_string(),
            );
        }
        // --- Verification (no mutation) ---
        let (installed, version_probe_detail) = self.installed_prisma_version_probe(ctx).await;
        let version_ok = installed.as_deref() == Some(crate::PRISMA_PINNED_VERSION);
        let client_present = ctx.ops.file_exists(&self.generated_client_path(ctx)).await;
        if version_ok && client_present {
            if self.prisma_probe_failure(ctx, database_url).await.is_none() {
                push_log(
                    ctx,
                    log,
                    "Prisma install verified (pinned version, generated client present, connect probe passed) — no changes made"
                        .to_string(),
                );
                // A verified install supersedes any interrupted
                // repair record.
                let _ = ctx
                    .ops
                    .remove_file(&Self::prisma_repair_state_path(ctx))
                    .await;
                return Ok(());
            }
            push_log(
                ctx,
                log,
                "Prisma verification failed at the connect probe — repairing the install"
                    .to_string(),
            );
        } else {
            push_log(
                ctx,
                log,
                format!(
                    "Prisma verification failed (installed version: {}, generated client: {}) — repairing the install",
                    match (&installed, &version_probe_detail) {
                        (Some(v), _) => v.clone(),
                        (None, Some(detail)) => format!("unknown — probe failed: {detail}"),
                        (None, None) => "unknown".to_string(),
                    },
                    if client_present { "present" } else { "missing" }
                ),
            );
        }
        // --- Repair (durable state first, then mutations) ---
        self.write_prisma_repair_state(ctx, "pin", log).await;
        self.ensure_prisma_pin(ctx, uv, log).await?;
        self.write_prisma_repair_state(ctx, "probe", log).await;
        if self.prisma_probe_failure(ctx, database_url).await.is_none() {
            log.push("Prisma client verified after the pin repair".to_string());
            let _ = ctx
                .ops
                .remove_file(&Self::prisma_repair_state_path(ctx))
                .await;
            return Ok(());
        }
        log.push(
            "Prisma client probe failed — generating the client before the gateway starts"
                .to_string(),
        );
        self.write_prisma_repair_state(ctx, "generate", log).await;
        let schema = self.resolve_prisma_schema(ctx, database_url, log).await?;
        log.push(format!(
            "generating the Prisma client from {}",
            schema.display()
        ));
        self.run_prisma_generate(ctx, database_url, &schema, log)
            .await?;
        self.write_prisma_repair_state(ctx, "verify", log).await;
        match self.prisma_probe_failure(ctx, database_url).await {
            None => {
                log.push("Prisma client verified after generation".to_string());
                let _ = ctx
                    .ops
                    .remove_file(&Self::prisma_repair_state_path(ctx))
                    .await;
                Ok(())
            }
            Some(detail) => Err(StepOutcome::failed(
                format!("Prisma client still does not connect after prisma generate: {detail}"),
                log.clone(),
            )),
        }
    }
}
