//! Step 2: install the llama.cpp CUDA build.

use super::*;

// ---------------------------------------------------------------------------
// 2. Install llama.cpp CUDA build
// ---------------------------------------------------------------------------

/// One archive of a pinned llama.cpp release: file name + expected SHA-256.
#[derive(Debug, Clone)]
pub struct LlamaCppAsset {
    pub asset: String,
    pub sha256: String,
}

/// Pinned llama.cpp build (spec §3.4).
///
/// The pin is compiled into the build: [`LlamaCppPin::default_pin`] carries
/// the tag and SHA-256 of both upstream archives (the binaries archive and
/// the companion CUDA-runtime archive — the binaries ship without the
/// cudart/cublas DLLs, so both are required). A release covers exactly one
/// tag; `updates.components.llamacpp.pinned_version` acts as a guard, not a
/// source: naming any other version fails closed in the step, because this
/// build carries no verified pin for it. [`LlamaCppPin::validate_for_gpus`]
/// enforces the Pascal rule — cuda-12.4 assets carry the sm_60 targets,
/// cuda-13.x dropped them.
#[derive(Debug, Clone)]
pub struct LlamaCppPin {
    pub tag: String,
    pub asset: String,
    pub sha256: String,
    /// Companion CUDA-runtime archive (cudart/cublas DLLs), same release.
    pub runtime: LlamaCppAsset,
}

impl LlamaCppPin {
    pub const BASE_URL: &'static str = "https://github.com/ggml-org/llama.cpp/releases/download";

    /// The verified pin shipped with this build: ggml-org llama.cpp
    /// **b11658**, cuda-12.4 x64. Both SHA-256 hashes were computed from
    /// the official release downloads on 2026-10-15 (not copied from a
    /// listing), and both archives were content-checked: the binaries
    /// archive carries `llama-server.exe` and `ggml-cuda.dll` at its root
    /// but no CUDA runtime DLLs; the runtime archive carries exactly
    /// `cublas64_12.dll`, `cublasLt64_12.dll`, and `cudart64_12.dll`.
    /// Both extract flat into the install dir.
    pub fn default_pin() -> Self {
        Self {
            tag: "b11658".to_string(),
            asset: "llama-b11658-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: "4268543cfffee11e62dbe1fb4ee907da508c2a70de7b845ed7b338bf8cabd76d".to_string(),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"
                    .to_string(),
            },
        }
    }

    pub fn download_url(&self) -> String {
        self.asset_url(&self.asset)
    }

    pub fn runtime_download_url(&self) -> String {
        self.asset_url(&self.runtime.asset)
    }

    fn asset_url(&self, asset: &str) -> String {
        format!("{}/{}/{}", Self::BASE_URL, self.tag, asset)
    }

    /// Both archives of the release, binaries first, in download order.
    pub(crate) fn archives(&self) -> [LlamaCppAsset; 2] {
        [
            LlamaCppAsset {
                asset: self.asset.clone(),
                sha256: self.sha256.clone(),
            },
            self.runtime.clone(),
        ]
    }

    /// Refuse cuda-13.x assets on Pascal-bearing machines (spec §3.4).
    /// Both archive names are checked: a cuda-13 runtime archive is just
    /// as unusable on Pascal as a cuda-13 binaries archive.
    pub fn validate_for_gpus(&self, gpus: &[crate::GpuDescriptor]) -> Result<(), WizardError> {
        let has_pascal = gpus.iter().any(|g| g.is_pascal_or_older());
        if !has_pascal {
            return Ok(());
        }
        for archive in self.archives() {
            let asset = archive.asset.to_lowercase();
            if asset.contains("cuda-13") || asset.contains("cuda13") {
                return Err(WizardError::UnsupportedBuild {
                    asset: archive.asset.clone(),
                    reason: "cuda-13.x builds dropped Pascal (sm_60) support; \
                             pin a cuda-12.4 asset (spec §3.4)"
                        .to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Step 2: download the pinned llama.cpp CUDA build (binaries archive +
/// CUDA-runtime archive), verify both SHA-256s, extract both flat into
/// the app dir. Audit mode re-verifies the installed build.
pub struct InstallLlamaCpp {
    pub pin: Option<LlamaCppPin>,
}

impl InstallLlamaCpp {
    pub fn unconfigured() -> Self {
        Self { pin: None }
    }
    pub fn with_pin(pin: LlamaCppPin) -> Self {
        Self { pin: Some(pin) }
    }

    fn install_dir(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("llamacpp")
    }

    /// The MSVC-built llama.cpp CUDA binaries need the VC++ runtime.
    /// Probe: vcruntime140.dll loadable from System32. Windows-only;
    /// other platforms always pass.
    pub(crate) fn vcredist_present(system32: &std::path::Path) -> bool {
        if !cfg!(target_os = "windows") {
            return true;
        }
        system32.join("vcruntime140.dll").exists()
    }

    fn system32_dir() -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        PathBuf::from(root).join("System32")
    }

    fn download_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        let Some(pin) = self.pin.as_ref() else {
            return Vec::new();
        };
        let dir = self.install_dir(ctx);
        pin.archives()
            .iter()
            .map(|archive| {
                PlannedCommand::new(vec![
                    "download".to_string(),
                    pin.asset_url(&archive.asset),
                    dir.join(&archive.asset).display().to_string(),
                ])
            })
            .collect()
    }
}

#[async_trait]
impl WizardStep for InstallLlamaCpp {
    fn id(&self) -> &str {
        STEP_INSTALL_LLAMACPP
    }
    fn name(&self) -> &str {
        "Install llama.cpp CUDA build"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        self.download_commands(ctx)
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run install-llamacpp to re-download and re-verify the build; if llama-server fails to start, install the VC++ redistributable first")
    }
    fn help_url(&self) -> Option<&str> {
        Some(crate::VC_REDIST_DOWNLOAD_URL)
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let pin = match &self.pin {
            Some(p) => p.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "no llama.cpp build pinned",
                    vec![
                        "this build ships without a built-in llama.cpp pin; that is a packaging error in the app, not a configuration problem".to_string(),
                        "failing closed: will not download an unpinned binary".to_string(),
                    ],
                ))
            }
        };
        // Config guard: updates.components.llamacpp.pinned_version may name
        // the version the operator expects, but this build carries a
        // verified pin for exactly one tag. A different version is refused
        // (fail closed), never silently ignored or downloaded unverified.
        if let Some(wanted) = ctx
            .config
            .updates
            .components
            .get("llamacpp")
            .and_then(|c| c.pinned_version.as_deref())
        {
            if wanted != pin.tag {
                return Ok(StepOutcome::failed(
                    format!(
                        "updates.components.llamacpp pins llama.cpp {wanted}, but this build carries a verified pin only for {}",
                        pin.tag
                    ),
                    vec![
                        format!(
                            "operator-pinned version: {wanted}; verified built-in pin: {}",
                            pin.tag
                        ),
                        "failing closed: will not download a build without a verified SHA-256 pin"
                            .to_string(),
                    ],
                ));
            }
        }
        // VC++ runtime check before any download: the MSVC-built CUDA
        // binaries fail to start without it, with a cryptic DLL error.
        if ctx.mode == RunMode::Install && !Self::vcredist_present(&Self::system32_dir()) {
            return Ok(StepOutcome::failed(
                "VC++ redistributable not detected (vcruntime140.dll missing from System32)",
                vec![
                    "the llama.cpp CUDA build needs the Visual C++ runtime".to_string(),
                    format!(
                        "download and install it, then re-run this step: {}",
                        crate::VC_REDIST_DOWNLOAD_URL
                    ),
                ],
            ));
        }
        // Pascal rule, checked before any download.
        let gpus = ctx.ops.gpu_inventory().await.unwrap_or_default();
        if let Err(e) = pin.validate_for_gpus(&gpus) {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }

        let dir = self.install_dir(ctx);
        let marker = dir.join(".installed.json");
        let archives = pin.archives();
        let mut log = vec![
            format!("pinned build: {} ({})", pin.asset, pin.tag),
            format!("pinned runtime: {} ({})", pin.runtime.asset, pin.tag),
        ];

        match ctx.mode {
            RunMode::Install => {
                // Both archives are required: the binaries archive has no
                // CUDA runtime DLLs, the runtime archive has no binaries.
                // Each is downloaded, SHA-256-verified, and extracted flat
                // into the install dir; an archive already on disk with a
                // matching checksum is kept (per-archive skip).
                for archive_pin in &archives {
                    let archive = dir.join(&archive_pin.asset);
                    let archive_ok = ctx.ops.file_exists(&archive).await
                        && matches!(ctx.ops.sha256_file(&archive).await, Ok(h) if checksums_match(&h, &archive_pin.sha256));
                    if !archive_ok {
                        let url = pin.asset_url(&archive_pin.asset);
                        push_log(ctx, &mut log, format!("downloading {url}"));
                        // The planned commands must equal the spawned
                        // actions: the downloads in this loop are exactly
                        // planned_commands()[0] and planned_commands()[1],
                        // in this order, when they are needed at all.
                        let progress = ProgressEmitter::new(ctx, archive_pin.asset.clone());
                        let dl_started = std::time::Instant::now();
                        match ctx
                            .ops
                            .download(&url, &archive, &|d, t| progress.progress(d, t))
                            .await
                        {
                            Ok(bytes) => push_log(
                                ctx,
                                &mut log,
                                format!("downloaded {bytes} bytes in {}", fmt_elapsed(dl_started)),
                            ),
                            Err(e) => {
                                return Ok(StepOutcome::failed(
                                    format!("{e}"),
                                    vec![format!("{e:?}")],
                                ))
                            }
                        }
                        let actual = match ctx.ops.sha256_file(&archive).await {
                            Ok(h) => h,
                            Err(e) => {
                                return Ok(StepOutcome::failed(
                                    format!("{e}"),
                                    vec![format!("{e:?}")],
                                ))
                            }
                        };
                        if !checksums_match(&actual, &archive_pin.sha256) {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "checksum mismatch for {}: expected {}, got {}",
                                    archive.display(),
                                    archive_pin.sha256,
                                    actual
                                ),
                                vec!["archive rejected; not installed".to_string()],
                            ));
                        }
                        log.push(format!("SHA-256 verified for {}", archive_pin.asset));
                    } else {
                        log.push(format!(
                            "{} already present with matching checksum",
                            archive_pin.asset
                        ));
                    }
                    push_log(
                        ctx,
                        &mut log,
                        format!("extracting {} to {}", archive_pin.asset, dir.display()),
                    );
                    let extract_started = std::time::Instant::now();
                    if let Err(e) = ctx.ops.extract_zip(&archive, &dir).await {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                    }
                    push_log(
                        ctx,
                        &mut log,
                        format!(
                            "extracted {} to {} in {}",
                            archive_pin.asset,
                            dir.display(),
                            fmt_elapsed(extract_started)
                        ),
                    );
                }
                let marker_json = serde_json::json!({
                    "tag": pin.tag,
                    "asset": pin.asset,
                    "sha256": pin.sha256,
                    "runtime_asset": pin.runtime.asset,
                    "runtime_sha256": pin.runtime.sha256,
                });
                if let Err(e) = ctx
                    .ops
                    .write_file(&marker, marker_json.to_string().as_bytes())
                    .await
                {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                Ok(StepOutcome::done(
                    format!("llama.cpp {} installed to {}", pin.tag, dir.display()),
                    log,
                ))
            }
            RunMode::Audit => {
                // Re-verify without reinstalling: marker + BOTH archive
                // checksums.
                if !ctx.ops.file_exists(&marker).await {
                    return Ok(StepOutcome::failed(
                        "llama.cpp install marker missing",
                        vec![format!("{} not found", marker.display())],
                    ));
                }
                for archive_pin in &archives {
                    let archive = dir.join(&archive_pin.asset);
                    match ctx.ops.sha256_file(&archive).await {
                        Ok(h) if checksums_match(&h, &archive_pin.sha256) => {}
                        Ok(h) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "installed build checksum mismatch for {}: expected {}, got {}",
                                    archive_pin.asset, archive_pin.sha256, h
                                ),
                                log,
                            ))
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(
                                format!("cannot read installed archive {}: {e}", archive.display()),
                                vec![format!("{e:?}")],
                            ))
                        }
                    }
                }
                Ok(StepOutcome::done(
                    format!(
                        "llama.cpp {} build verified ({} + {})",
                        pin.tag, pin.asset, pin.runtime.asset
                    ),
                    log,
                ))
            }
        }
    }
}
