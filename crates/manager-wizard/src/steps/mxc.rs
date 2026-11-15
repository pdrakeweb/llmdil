//! Steps 10-11: install the MXC runtime and apply MXC policies.

use super::*;

// ---------------------------------------------------------------------------
// 10. Install MXC runtime
// ---------------------------------------------------------------------------

/// Verified pin for the Microsoft MXC SDK distribution the wizard
/// installs: `@microsoft/mxc-sdk` 1.0.0 (published 2026-10-06). The
/// package has no install scripts and no PATH shims — the runtime is
/// payload the SDK locates relative to its own package root — so the
/// step downloads the npm tarball itself, verifies its SHA-256,
/// extracts only `package/bin/x64/` (the whole folder: wxc-exec.exe
/// resolves sibling daemons and DLLs from its own directory at
/// runtime) into `<data_dir>/mxc/bin/x64/`, and verifies both named
/// executables against their pins. All hashes were computed from the
/// actual registry tarball on 2026-10-15 (tarball SHA-256
/// 39e59ffd…; registry SHA-1 5e211c18… cross-checked).
#[derive(Debug, Clone)]
pub struct MxcSdkPin {
    pub version: String,
    pub tarball_asset: String,
    pub tarball_sha256: String,
    pub tarball_size: u64,
    pub wxc_exec_sha256: String,
    pub wxc_host_prep_sha256: String,
}

impl MxcSdkPin {
    pub const TARBALL_BASE: &'static str = "https://registry.npmjs.org/@microsoft/mxc-sdk/-";

    pub fn default_pin() -> Self {
        Self {
            version: "1.0.0".to_string(),
            tarball_asset: "mxc-sdk-1.0.0.tgz".to_string(),
            tarball_sha256: "39e59ffd9d0832334889b0cd583d0beb43b1bbbe9f783dd7c6980203d23e9602"
                .to_string(),
            tarball_size: 37_746_375,
            wxc_exec_sha256: "8a6d1db4dd1846981127d9014741be18b094d07be967765dff6754bda1d40259"
                .to_string(),
            wxc_host_prep_sha256:
                "c07c94acf62844c5c20590a2d3e7944d0beaaf424df934afc8c83d531bf72825".to_string(),
        }
    }

    pub fn tarball_url(&self) -> String {
        format!("{}/{}", Self::TARBALL_BASE, self.tarball_asset)
    }
}

/// The app-installed MXC runtime directory: `<data_dir>/mxc/bin/x64`,
/// mirroring the SDK package layout (`bin/<arch>/`).
fn mxc_bin_dir(ctx: &WizardContext) -> PathBuf {
    ctx.data_dir.join("mxc").join("bin").join("x64")
}

/// Absolute path of the app-installed `wxc-exec.exe`.
pub fn mxc_wxc_exec_path(ctx: &WizardContext) -> PathBuf {
    mxc_bin_dir(ctx).join("wxc-exec.exe")
}

fn mxc_host_prep_path(ctx: &WizardContext) -> PathBuf {
    mxc_bin_dir(ctx).join("wxc-host-prep.exe")
}

/// Resolve the wxc-exec invocation for the MXC steps: the
/// app-installed runtime when install-mxc has placed it, else the
/// bare program name (a system install on PATH).
async fn resolve_wxc_exec(ctx: &WizardContext) -> String {
    let installed = mxc_wxc_exec_path(ctx);
    if ctx.ops.file_exists(&installed).await {
        installed.display().to_string()
    } else {
        "wxc-exec.exe".to_string()
    }
}

/// Step 10: install the MXC runtime (pinned `@microsoft/mxc-sdk`
/// binaries) under the app data dir and run the one-time elevated
/// host preparation. MXC is installed for setup completeness — Pete's
/// decision (2026-10-15): nothing in the app routes MCP launches
/// through wxc-exec yet, so the policy step that follows records
/// configuration only.
pub struct InstallMxc {
    pub pin: Option<MxcSdkPin>,
}

impl InstallMxc {
    pub fn unconfigured() -> Self {
        Self { pin: None }
    }
    pub fn with_pin(pin: MxcSdkPin) -> Self {
        Self { pin: Some(pin) }
    }

    fn install_root(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mxc")
    }

    fn marker_path(&self, ctx: &WizardContext) -> PathBuf {
        self.install_root(ctx).join(".installed.json")
    }

    fn tarball_path(&self, ctx: &WizardContext, pin: &MxcSdkPin) -> PathBuf {
        self.install_root(ctx)
            .join("downloads")
            .join(&pin.tarball_asset)
    }

    fn prep_script_path(&self, ctx: &WizardContext) -> PathBuf {
        self.install_root(ctx).join("host-prep.cmd")
    }

    /// The elevated launch that performs host preparation: a small
    /// script chaining both wxc-host-prep subcommands, so the user
    /// sees exactly one UAC prompt. `prepare-system-drive` persists
    /// across reboots; `prepare-null-device` reapplies the
    /// `\Device\Null` security descriptor, which the kernel resets
    /// every boot — it is re-run here so a completed setup works
    /// immediately, and will need re-applying before sandboxed runs
    /// after a reboot (owned by the future enforcement phase, not
    /// this setup step).
    fn elevated_prep_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            self.prep_script_path(ctx).display().to_string(),
        ])
    }

    fn prep_script_contents(&self, ctx: &WizardContext) -> String {
        let prep = mxc_host_prep_path(ctx).display().to_string();
        format!(
            "@echo off\r\n\"{prep}\" prepare-system-drive --quiet\r\n\"{prep}\" prepare-null-device --quiet\r\n"
        )
    }

    async fn binaries_verified(&self, ctx: &WizardContext, pin: &MxcSdkPin) -> bool {
        let exec_ok = matches!(
            ctx.ops.sha256_file(&mxc_wxc_exec_path(ctx)).await,
            Ok(h) if checksums_match(&h, &pin.wxc_exec_sha256)
        );
        let prep_ok = matches!(
            ctx.ops.sha256_file(&mxc_host_prep_path(ctx)).await,
            Ok(h) if checksums_match(&h, &pin.wxc_host_prep_sha256)
        );
        exec_ok && prep_ok
    }

    async fn read_marker(&self, ctx: &WizardContext) -> Option<serde_json::Value> {
        let raw = ctx.ops.read_file(&self.marker_path(ctx)).await.ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn marker_prep_done(marker: &Option<serde_json::Value>) -> bool {
        marker
            .as_ref()
            .and_then(|m| m.get("host_prep"))
            .map(|p| {
                p.get("system_drive").and_then(|v| v.as_bool()) == Some(true)
                    && p.get("null_device").and_then(|v| v.as_bool()) == Some(true)
            })
            .unwrap_or(false)
    }

    async fn write_marker(
        &self,
        ctx: &WizardContext,
        pin: &MxcSdkPin,
        prep_done: bool,
    ) -> Result<(), WizardError> {
        let marker = serde_json::json!({
            "version": pin.version,
            "tarball_sha256": pin.tarball_sha256,
            "wxc_exec_sha256": pin.wxc_exec_sha256,
            "wxc_host_prep_sha256": pin.wxc_host_prep_sha256,
            "host_prep": {
                "system_drive": prep_done,
                "null_device": prep_done,
                "at": chrono::Utc::now().to_rfc3339(),
            },
        });
        ctx.ops
            .write_file(&self.marker_path(ctx), json_pretty(&marker).as_bytes())
            .await
    }
}

#[async_trait]
impl WizardStep for InstallMxc {
    fn id(&self) -> &str {
        STEP_INSTALL_MXC
    }
    fn name(&self) -> &str {
        "Install MXC runtime"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        let Some(pin) = self.pin.as_ref() else {
            return Vec::new();
        };
        vec![
            PlannedCommand::new(vec![
                "download".to_string(),
                pin.tarball_url(),
                self.tarball_path(ctx, pin).display().to_string(),
            ]),
            self.elevated_prep_command(ctx),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run install-mxc and accept the Windows elevation prompt so wxc-host-prep can prepare the host")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let pin = match &self.pin {
            Some(p) => p.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "no MXC SDK build pinned",
                    vec![
                        "this build ships without a built-in MXC SDK pin; that is a packaging error in the app, not a configuration problem".to_string(),
                        "failing closed: will not download an unpinned runtime".to_string(),
                    ],
                ))
            }
        };
        let root = self.install_root(ctx);
        let mut log = vec![format!(
            "pinned MXC SDK: @microsoft/mxc-sdk@{} ({})",
            pin.version, pin.tarball_asset
        )];

        match ctx.mode {
            RunMode::Audit => {
                if !ctx.ops.file_exists(&self.marker_path(ctx)).await {
                    return Ok(StepOutcome::failed(
                        "MXC runtime not installed; run the install-mxc step",
                        vec![format!("{} not found", self.marker_path(ctx).display())],
                    ));
                }
                if !self.binaries_verified(ctx, &pin).await {
                    return Ok(StepOutcome::failed(
                        format!(
                            "installed MXC binaries fail checksum verification against the pinned SDK {}",
                            pin.version
                        ),
                        log,
                    ));
                }
                // Host preparation is deliberately not audited: the
                // null-device preparation resets every boot by design,
                // so an audit after any reboot would always fail.
                return Ok(StepOutcome::done(
                    format!("MXC runtime {} verified", pin.version),
                    log,
                ));
            }
            RunMode::Install => {}
        }

        // Install the binaries unless the pinned pair already verifies.
        if self.binaries_verified(ctx, &pin).await {
            log.push("MXC binaries already installed with matching checksums".to_string());
        } else {
            let tarball = self.tarball_path(ctx, &pin);
            let tarball_ok = ctx.ops.file_exists(&tarball).await
                && matches!(ctx.ops.sha256_file(&tarball).await, Ok(h) if checksums_match(&h, &pin.tarball_sha256));
            if !tarball_ok {
                let url = pin.tarball_url();
                push_log(ctx, &mut log, format!("downloading {url}"));
                let progress = ProgressEmitter::new(ctx, pin.tarball_asset.clone());
                let dl_started = std::time::Instant::now();
                match ctx
                    .ops
                    .download(&url, &tarball, &|d, t| progress.progress(d, t))
                    .await
                {
                    Ok(bytes) => push_log(
                        ctx,
                        &mut log,
                        format!("downloaded {bytes} bytes in {}", fmt_elapsed(dl_started)),
                    ),
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                let actual = match ctx.ops.sha256_file(&tarball).await {
                    Ok(h) => h,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                if !checksums_match(&actual, &pin.tarball_sha256) {
                    return Ok(StepOutcome::failed(
                        format!(
                            "checksum mismatch for {}: expected {}, got {}",
                            tarball.display(),
                            pin.tarball_sha256,
                            actual
                        ),
                        vec!["tarball rejected; not installed".to_string()],
                    ));
                }
                log.push(format!("SHA-256 verified for {}", pin.tarball_asset));
            } else {
                log.push(format!(
                    "{} already present with matching checksum",
                    pin.tarball_asset
                ));
            }
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracting {} to {}",
                    pin.tarball_asset,
                    mxc_bin_dir(ctx).display()
                ),
            );
            let extract_started = std::time::Instant::now();
            if let Err(e) = ctx
                .ops
                .extract_tar_gz(&tarball, &root, "package/bin/x64/", "package/")
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracted package/bin/x64 to {} in {}",
                    mxc_bin_dir(ctx).display(),
                    fmt_elapsed(extract_started)
                ),
            );
            if !self.binaries_verified(ctx, &pin).await {
                return Ok(StepOutcome::failed(
                    "extraction did not produce the pinned wxc executables",
                    vec![
                        format!("expected {} and {}", mxc_wxc_exec_path(ctx).display(), mxc_host_prep_path(ctx).display()),
                        "the tarball verified against its pin, so its layout differs from the pinned SDK's — failing closed".to_string(),
                    ],
                ));
            }
        }

        // One-time elevated host preparation. Skipped when a previous
        // run already recorded it — re-prompting UAC on every wizard
        // run would be hostile, and prepare-system-drive persists.
        let marker = self.read_marker(ctx).await;
        if Self::marker_prep_done(&marker) {
            log.push("host preparation already recorded; elevation not requested".to_string());
            return Ok(StepOutcome::done(
                format!(
                    "MXC runtime {} installed to {}; host preparation complete",
                    pin.version,
                    mxc_bin_dir(ctx).display()
                ),
                log,
            ));
        }
        if let Err(e) = ctx
            .ops
            .write_file(
                &self.prep_script_path(ctx),
                self.prep_script_contents(ctx).as_bytes(),
            )
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        let prep_note = match ctx.ops.run_elevated(&self.elevated_prep_command(ctx)).await {
            Ok(out) if out.success() => {
                log.push(
                    "host preparation completed (prepare-system-drive, prepare-null-device)"
                        .to_string(),
                );
                None
            }
            Ok(out) => Some(format!(
                "host preparation exited with code {}",
                out.exit_code
            )),
            Err(e) => Some(format!("host preparation was not completed: {e}")),
        };
        match prep_note {
            None => {
                if let Err(e) = self.write_marker(ctx, &pin, true).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                Ok(StepOutcome::done(
                    format!(
                        "MXC runtime {} installed to {}; host preparation complete",
                        pin.version,
                        mxc_bin_dir(ctx).display()
                    ),
                    log,
                ))
            }
            Some(note) => {
                // The runtime itself is installed and verified; only
                // the elevated preparation is outstanding. Record it
                // as not-done so a re-run retries the elevation
                // without re-downloading, and say so in the message —
                // the AppContainer tier stays unavailable until then.
                if let Err(e) = self.write_marker(ctx, &pin, false).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(note.clone());
                Ok(StepOutcome::done(
                    format!(
                        "MXC runtime {} installed to {}; {note} — re-run this step and accept the elevation prompt to finish host preparation",
                        pin.version,
                        mxc_bin_dir(ctx).display()
                    ),
                    log,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 11. Apply MXC policies
// ---------------------------------------------------------------------------

/// Step 11: probe MXC, write the default policy JSON, validate it,
/// run a harmless sandboxed self-test.
pub struct MxcPolicies;

impl MxcPolicies {
    fn policy_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mxc").join("policy.json")
    }

    /// The probe is `wxc-exec --probe`: "run the fallback detector
    /// and emit JSON, without spawning a sandbox" — the SDK's own
    /// invocation, verified against the pinned 1.0.0 binary. An
    /// earlier `--probe-json` form does not exist in that binary.
    fn probe_command(exe: &str) -> PlannedCommand {
        PlannedCommand::new(vec![exe.to_string(), "--probe".to_string()])
    }

    /// A self-test is the pinned policy loaded in `--dry-run` mode
    /// (config parse + tier selection, nothing spawned) with a
    /// harmless command attached, per the 1.0.0 CLI contract:
    /// `wxc-exec [--config <policy>] [--dry-run] [-- <command>]`. The
    /// earlier `--policy ... --self-test` form does not exist in the
    /// real binary.
    fn self_test_command(&self, ctx: &WizardContext, exe: &str) -> PlannedCommand {
        PlannedCommand::new(vec![
            exe.to_string(),
            "--config".to_string(),
            self.policy_path(ctx).display().to_string(),
            "--dry-run".to_string(),
            "--".to_string(),
            "cmd.exe".to_string(),
            "/c".to_string(),
            "echo llm-manager-mxc-self-test".to_string(),
        ])
    }
}

#[async_trait]
impl WizardStep for MxcPolicies {
    fn id(&self) -> &str {
        STEP_MXC_POLICIES
    }
    fn name(&self) -> &str {
        "Apply MXC policies"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Planned commands name the canonical app-installed runtime;
        // run() resolves the same path when it exists and falls back
        // to a PATH install otherwise.
        let exe = mxc_wxc_exec_path(ctx).display().to_string();
        match ctx.mode {
            RunMode::Install => {
                vec![Self::probe_command(&exe), self.self_test_command(ctx, &exe)]
            }
            RunMode::Audit => vec![Self::probe_command(&exe)],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("verify the MXC runtime is installed (run install-mxc), then re-run mxc-policies")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        let exe = resolve_wxc_exec(ctx).await;
        // Probe PSEC/ingress availability.
        match ctx.ops.run_command(&Self::probe_command(&exe)).await {
            Ok(out) if out.success() => {
                log.push("MXC probe succeeded".to_string());
                let stdout = out.stdout.trim();
                if !stdout.is_empty() {
                    log.push(format!(
                        "probe: {}",
                        stdout.chars().take(300).collect::<String>()
                    ));
                }
            }
            Ok(out) => {
                return Ok(StepOutcome::failed(
                    format!("MXC probe failed (exit {})", out.exit_code),
                    vec![out.stderr],
                ))
            }
            Err(e) if spawn_failed(&e) => {
                // The MXC runtime is a separate Microsoft distribution
                // (@microsoft/mxc-sdk); it is not installed by this
                // app. Absent runtime = optional component missing,
                // a skip — the same treatment as a missing VS Code.
                return Ok(StepOutcome::skipped(
                    "MXC runtime (wxc-exec.exe) is not installed; MXC policy application skipped",
                ));
            }
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("MXC runtime not available: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        }
        let mode = ctx.config.mxc.mode;
        let path = self.policy_path(ctx);
        match ctx.mode {
            RunMode::Install => {
                let policy = manager_mxc::Policy::default_policy();
                let report = manager_mxc::validate(&policy)
                    .map_err(|e| WizardError::Mxc(format!("policy validation error: {e}")))?;
                if !report.valid {
                    return Ok(StepOutcome::failed(
                        "default MXC policy failed validation".to_string(),
                        report.errors,
                    ));
                }
                for w in &report.warnings {
                    log.push(format!("policy warning: {w}"));
                }
                let json = serde_json::to_string_pretty(&policy)
                    .map_err(|e| WizardError::Mxc(format!("policy serialization failed: {e}")))?;
                if let Err(e) = ctx.ops.write_file(&path, json.as_bytes()).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(format!("wrote default policy to {}", path.display()));
                log.push(format!("mode: {mode:?} (Learning recommended first)"));
                // Harmless sandboxed self-test; activity shown inline.
                match ctx
                    .ops
                    .run_command(&self.self_test_command(ctx, &exe))
                    .await
                {
                    Ok(out) if out.success() => {
                        log.push("policy self-test passed".to_string());
                        if !out.stdout.trim().is_empty() {
                            log.push(format!(
                                "self-test: {}",
                                out.stdout.trim().chars().take(300).collect::<String>()
                            ));
                        }
                    }
                    Ok(out) => {
                        return Ok(StepOutcome::failed(
                            format!("MXC self-test failed (exit {})", out.exit_code),
                            vec![out.stderr],
                        ))
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("MXC self-test error: {e}"),
                            vec![format!("{e:?}")],
                        ))
                    }
                }
                Ok(StepOutcome::done(
                    format!("MXC policy applied ({mode:?} mode)"),
                    log,
                ))
            }
            RunMode::Audit => {
                // Re-probe + re-validate the on-disk policy; no reinstall.
                let data = match ctx.ops.read_file(&path).await {
                    Ok(d) => d,
                    Err(_) => {
                        return Ok(StepOutcome::failed(
                            "MXC policy.json missing; re-run mxc-policies".to_string(),
                            vec![format!("{} not found", path.display())],
                        ))
                    }
                };
                let policy: manager_mxc::Policy = match serde_json::from_slice(&data) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("policy.json unparseable: {e}"),
                            vec![],
                        ))
                    }
                };
                let report = manager_mxc::validate(&policy)
                    .map_err(|e| WizardError::Mxc(format!("policy validation error: {e}")))?;
                if !report.valid {
                    return Ok(StepOutcome::failed(
                        "installed MXC policy failed validation".to_string(),
                        report.errors,
                    ));
                }
                Ok(StepOutcome::done(
                    format!("MXC policy valid ({mode:?} mode)"),
                    vec![format!("validated {}", path.display())],
                ))
            }
        }
    }
}
