//! `StartGateway`: the managed Python env (uv bootstrap, venv, LiteLLM install and probes).

use super::*;

impl StartGateway {
    pub(crate) fn venv_dir(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("python").join("venv")
    }

    /// The commands to start the gateway by hand, attached to this
    /// step's failure outcomes ([`StepOutcome::manual_command`]) so
    /// an operator whose start failed can run them in PowerShell —
    /// getting a gateway up and seeing its errors directly — instead
    /// of being stuck behind the wizard. Contents, in run order:
    /// the app-managed PostgreSQL start line (only when PostgreSQL
    /// is installed), the secret environment the spawn needs (names
    /// and where the values live — Windows Credential Manager —
    /// never the values), and the exact spawn command
    /// [`Self::spawn_command`] builds, with this install's real
    /// paths, host, and port. Built once per run: whether PostgreSQL
    /// is installed cannot change during this step (installing it is
    /// a different step).
    pub(crate) async fn manual_start_block(&self, ctx: &WizardContext) -> String {
        let mut lines = vec![
            "# Start the gateway manually (PowerShell):".to_string(),
            "# Secret values are read from Windows Credential Manager; they are never written to disk."
                .to_string(),
        ];
        if postgres_installed(ctx.ops.as_ref(), &ctx.data_dir).await {
            let pg = postgres_start_command(&ctx.data_dir, ctx.config.postgres.port);
            lines.push(ps_command_line(&pg.argv));
            // The REAL port, not a placeholder: a block that says
            // "paste this and it works" must not hand the operator a
            // <port> token to hunt down (code-review 2.6).
            lines.push(format!(
                "$env:DATABASE_URL = \"postgresql://postgres:<password>@127.0.0.1:{}/{POSTGRES_DATABASE}\"  # <password>: Credential Manager '{}'",
                ctx.config.postgres.port,
                crate::POSTGRES_PASSWORD_CRED_TARGET
            ));
            // Same watchdog disable the app spawn sets (see
            // StartGateway::prisma_watchdog_must_be_disabled): a
            // manual start on Windows with a database otherwise
            // dies about a second after startup, silently.
            lines.push("$env:PRISMA_HEALTH_WATCHDOG_ENABLED = \"false\"".to_string());
        }
        lines.push(format!(
            "$env:LITELLM_MASTER_KEY = \"<value from Windows Credential Manager: '{}'>\"",
            crate::LITELLM_MASTER_KEY_CRED_TARGET
        ));
        let tier = &ctx.config.gateway.openrouter;
        if tier.enabled {
            if let Some(cred_ref) = tier.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                lines.push(format!(
                    "$env:OPENROUTER_API_KEY = \"<value from Windows Credential Manager: '{cred_ref}'>\""
                ));
            }
        }
        // MCP gateway servers (design section 4.2): a manual start must
        // behave like an app start -- the stdio flag when any
        // Gateway-exposed stdio server is configured (the manual
        // block is config-derived, like the plan; a server whose
        // credential is missing will simply not load, as at an app
        // start where it is not armed), plus a credential note per
        // Gateway server naming each env key it feeds. Values are
        // never shown, mirroring the master-key/OpenRouter lines.
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
            lines.push("$env:LITELLM_ENABLE_MCP_STDIO = \"true\"".to_string());
        }
        for server in &gateway_mcp {
            if let Some(cred_ref) = server.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                for key in &server.env_keys {
                    lines.push(format!(
                        "$env:{key} = \"<value from Windows Credential Manager: '{cred_ref}'>\"  # MCP server '{}'",
                        server.id
                    ));
                }
            }
        }
        // SearXNG search provider (design §6.3): the instance URL is
        // config, not a secret, so the manual block carries the real
        // value — a manual start must behave like an app start.
        let searxng = &ctx.config.mcp.searxng;
        if searxng.enabled && !searxng.api_base.trim().is_empty() {
            lines.push(format!(
                "$env:SEARXNG_API_BASE = \"{}\"",
                searxng.api_base.trim()
            ));
        }
        let spawn = self.spawn_command(ctx);
        // The spawn's own environment is not secret (UTF-8 stdio
        // forcing); emit it exactly as the step sets it.
        let mut env: Vec<(&String, &String)> = spawn.env.iter().collect();
        env.sort();
        for (k, v) in env {
            lines.push(format!("$env:{k} = \"{v}\""));
        }
        lines.push(ps_command_line(&spawn.argv));
        lines.join("\n")
    }

    /// Directory holding the bootstrapped uv binary.
    pub(crate) fn uv_bin_dir(ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("bin")
    }

    pub(crate) fn uv_binary_name() -> &'static str {
        if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        }
    }

    fn bundled_uv(ctx: &WizardContext) -> PathBuf {
        Self::uv_bin_dir(ctx).join(Self::uv_binary_name())
    }

    /// Find the uv binary in `dirs`. Pure and unit-testable.
    pub(crate) fn find_uv_in(dirs: &[PathBuf]) -> Option<PathBuf> {
        let name = Self::uv_binary_name();
        dirs.iter().map(|d| d.join(name)).find(|p| p.is_file())
    }

    /// `uv` on the real process PATH.
    fn uv_on_path() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        Self::find_uv_in(&dirs)
    }

    /// Synchronously resolve the uv argv0 for display, plus whether run()
    /// will need to bootstrap it first.
    pub(crate) fn uv_argv0(ctx: &WizardContext) -> (PathBuf, bool) {
        if let Some(p) = Self::uv_on_path() {
            return (p, false);
        }
        let bundled = Self::bundled_uv(ctx);
        if bundled.is_file() {
            return (bundled, false);
        }
        // Not resolvable synchronously: run() bootstraps it on Windows;
        // elsewhere the bare name documents the PATH requirement.
        (bundled, crate::uv_download_url().is_some())
    }

    pub(crate) fn uv_archive_name() -> String {
        format!("uv-{}.zip", crate::UV_PINNED_VERSION)
    }

    /// Resolve a usable `uv`: PATH first, then the bootstrapped copy under
    /// data_dir/bin, else download + extract the pinned standalone build
    /// (Windows only; other platforms fail with install guidance).
    pub(crate) async fn ensure_uv(&self, ctx: &WizardContext) -> Result<PathBuf, StepOutcome> {
        if let Some(p) = Self::uv_on_path() {
            return Ok(p);
        }
        let bundled = Self::bundled_uv(ctx);
        if ctx.ops.file_exists(&bundled).await {
            return Ok(bundled);
        }
        let url = match crate::uv_download_url() {
            Some(u) => u,
            None => {
                return Err(StepOutcome::failed(
                    "uv not found on PATH and auto-bootstrap is only supported on Windows",
                    vec![format!(
                        "install uv from {} and re-run this step",
                        crate::UV_INSTALL_DOCS_URL
                    )],
                ))
            }
        };
        let dir = Self::uv_bin_dir(ctx);
        let archive = dir.join(Self::uv_archive_name());
        // ensure_uv has no step log of its own — the download's
        // progress goes to the live sink only.
        ctx.emit_log(&format!("downloading {url} (uv bootstrap)"));
        let progress = ProgressEmitter::new(ctx, "uv");
        let dl_started = std::time::Instant::now();
        ctx.ops
            .download(&url, &archive, &|d, t| progress.progress(d, t))
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!("uv bootstrap download failed: {e}"),
                    vec![format!("{e:?}")],
                )
            })?;
        ctx.emit_log(&format!(
            "uv bootstrap downloaded in {}",
            fmt_elapsed(dl_started)
        ));
        ctx.emit_log(&format!(
            "extracting the uv bootstrap archive to {}",
            dir.display()
        ));
        let extract_started = std::time::Instant::now();
        ctx.ops.extract_zip(&archive, &dir).await.map_err(|e| {
            StepOutcome::failed(
                format!("uv bootstrap extraction failed: {e}"),
                vec![format!("{e:?}")],
            )
        })?;
        ctx.emit_log(&format!(
            "uv bootstrap extracted in {}",
            fmt_elapsed(extract_started)
        ));
        Ok(bundled)
    }

    pub(crate) fn venv_python(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }

    pub(crate) fn gateway_url(&self, ctx: &WizardContext) -> String {
        format!(
            "http://{}:{}/v1/models",
            gateway_probe_host(&ctx.config),
            ctx.config.gateway.port
        )
    }

    /// The pip requirement for the gateway venv. Always version-pinned
    /// (finding F1, `docs/code-review-v0.1.60.md`): the operator's
    /// `updates.components["litellm"].pinned_version` when set, else
    /// [`manager_config::DEFAULT_LITELLM_PIN`]. Loaded configs get the
    /// default backfilled by `manager_config::migrate`; the fallback here
    /// covers a context built from a config that never went through it,
    /// so no code path installs whatever PyPI serves that day.
    pub(crate) fn litellm_spec(&self, ctx: &WizardContext) -> String {
        format!("litellm[proxy]=={}", self.litellm_pinned_version(ctx))
    }

    /// The LiteLLM version the venv must carry: the operator pin, else
    /// [`manager_config::DEFAULT_LITELLM_PIN`] (see [`Self::litellm_spec`]).
    pub(crate) fn litellm_pinned_version(&self, ctx: &WizardContext) -> String {
        ctx.config
            .updates
            .components
            .get(manager_config::LITELLM_COMPONENT)
            .and_then(|c| c.pinned_version.as_deref())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(manager_config::DEFAULT_LITELLM_PIN)
            .to_string()
    }

    pub(crate) fn venv_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
        // --python pins the interpreter (see GATEWAY_PYTHON_VERSION);
        // --clear makes re-runs idempotent: a venv left behind by a
        // failed install or built with the wrong Python is replaced
        // instead of failing with "already exists".
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "venv".to_string(),
            "--python".to_string(),
            crate::GATEWAY_PYTHON_VERSION.to_string(),
            "--clear".to_string(),
            self.venv_dir(ctx).display().to_string(),
        ])
    }

    /// Probe printing the venv interpreter's "major.minor" version.
    pub(crate) fn version_probe_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')".to_string(),
        ])
    }

    /// Probe that succeeds only when litellm imports in the venv.
    pub(crate) fn litellm_probe_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            // prisma and prometheus_client ride along: a venv that
            // predates either addition to the install spec (see
            // pip_command) must not pass this probe, so the pip step
            // runs and completes it.
            "import litellm, prisma, prometheus_client".to_string(),
        ])
    }

    /// The interpreter probe WITH its evidence: on failure the
    /// second element says why (no interpreter, spawn error, exit
    /// code + output tail, or output with no version in it), so
    /// the caller can record the actual defect in the step log
    /// before choosing a repair. Probes used to answer a bare
    /// bool/None, which made a corrupted venv indistinguishable
    /// from a missing one — and the repair for "missing" is the
    /// full rebuild. Bounded by [`PROBE_COMMAND_TIMEOUT`].
    pub(crate) async fn venv_python_version_probe(
        &self,
        ctx: &WizardContext,
    ) -> (Option<String>, Option<String>) {
        let python = self.venv_python(ctx);
        if !ctx.ops.file_exists(&python).await {
            return (None, Some("the venv interpreter does not exist".into()));
        }
        match ctx
            .ops
            .run_command_with_timeout(&self.version_probe_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => match parse_two_part_version(&out.stdout) {
                Some(v) => (Some(v), None),
                None => (
                    None,
                    Some(format!(
                        "no version in the interpreter's output: {}",
                        output_tail_lines(out.stdout.trim(), 2).join(" | ")
                    )),
                ),
            },
            Ok(out) => (None, Some(probe_failure_detail(&out))),
            Err(e) => (None, Some(format!("the interpreter could not run: {e}"))),
        }
    }

    /// The import probe WITH its evidence (see
    /// [`Self::venv_python_version_probe`]): on failure the second
    /// element carries the import error itself — the line that
    /// distinguishes a missing package from a corrupted one.
    pub(crate) async fn litellm_import_probe(&self, ctx: &WizardContext) -> (bool, Option<String>) {
        match ctx
            .ops
            .run_command_with_timeout(&self.litellm_probe_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => (true, None),
            Ok(out) => (false, Some(probe_failure_detail(&out))),
            Err(e) => (false, Some(format!("the interpreter could not run: {e}"))),
        }
    }

    pub(crate) fn pip_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            self.venv_python(ctx).display().to_string(),
            self.litellm_spec(ctx),
            // prisma is not part of litellm[proxy] (it sits in the
            // separate extra-proxy extra), but the proxy's auth
            // exception handler imports it unconditionally when
            // classifying an authentication error — without it an
            // unauthenticated request returns HTTP 500 (the handler
            // itself crashes on ModuleNotFoundError) instead of a
            // clean 401. Found on real hardware 2026-10-15. The
            // version is pinned exactly: prisma-client-py >= 0.13
            // breaks LiteLLM's PrismaWrapper (see
            // crate::PRISMA_PINNED_VERSION).
            format!("prisma=={}", crate::PRISMA_PINNED_VERSION),
            // prometheus-client is only in litellm's proxy-runtime
            // extra. The generated gateway config enables the
            // prometheus success callback (dashboard request/error
            // stats), and LiteLLM fails to start when the callback
            // is set but this package is missing — the mount is
            // unguarded during config load. Verified 2026-10-15.
            "prometheus-client".to_string(),
        ])
    }

    /// The venv's Python interpreter, used to run the gateway.
    ///
    /// The gateway is NOT spawned through the venv's `litellm.exe`
    /// console script. That file is a launcher (uv/pip trampoline):
    /// it spawns the real python.exe as its own child, forwarding
    /// neither our CREATE_NO_WINDOW flag nor our console — and
    /// because the launcher itself has no console, Windows allocates
    /// a fresh console window for python.exe, which stays on the
    /// desktop for the gateway's whole lifetime (found on real
    /// hardware 2026-10-15; the same indirection also made the
    /// recorded gateway pid the launcher's, not the server's).
    /// Spawning the interpreter directly keeps one process level:
    /// the creation flags hold, no window appears, and the pid is
    /// the server itself.
    ///
    /// On Windows the console-subsystem interpreter (python.exe)
    /// is used, NOT pythonw.exe: a GUI-subsystem process cannot own
    /// a console, so every console-subsystem child it spawns — the
    /// Prisma query engine the gateway's database client starts —
    /// gets a fresh visible console window from Windows, for the
    /// gateway's lifetime. python.exe spawned with CREATE_NO_WINDOW
    /// alone gets a console that exists but has no window, and
    /// console children inherit that hidden console instead of
    /// opening their own. The gateway itself stays invisible either
    /// way (its stdio is redirected to the gateway log files), but
    /// only the python.exe shape keeps its children invisible too.
    /// This reverses the 2026-10-15 pythonw preference on that
    /// evidence: pythonw solved the gateway's own window and
    /// created its children's.
    pub(crate) fn gateway_python(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(windows) {
            // python.exe, not pythonw: see the comment above — the
            // GUI-subsystem interpreter cannot hand a hidden console
            // to the gateway's console children.
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }
}
