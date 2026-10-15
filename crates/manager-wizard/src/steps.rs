//! The ten concrete wizard steps (spec §4.1).
//!
//! Steps branch on [`RunMode`]: in `Install` they perform the install; in
//! `Audit` they re-verify without reinstalling. All system effects go through
//! [`SystemOps`](crate::SystemOps), so every step is hermetic under
//! [`FakeSystemOps`](crate::fake::FakeSystemOps).

use crate::{
    checksums_match, is_pinned_hash, PlannedCommand, RunMode, StepOutcome, WizardContext,
    WizardError, WizardStep,
};
use async_trait::async_trait;
use manager_config::{AppConfig, ModelConfig};
use std::path::{Path, PathBuf};

/// Generate the Continue `config.json` content with every gateway model
/// group as a model pointing at the local LiteLLM gateway.
///
/// Shared by the [`VscodeExtensions`] wizard step and the Tauri
/// `vscode_write_config` command so both write byte-identical configs.
/// When `master_key` is given it is written as each model's `apiKey`:
/// the gateway requires it, and an editor client config is where such
/// a key conventionally lives (the user's own profile, on their own
/// machine).
pub fn continue_editor_config(config: &AppConfig, master_key: Option<&str>) -> String {
    let models: Vec<serde_json::Value> = config
        .gateway
        .groups
        .iter()
        .map(|g| {
            let mut m = serde_json::json!({
                "title": format!("{} (local)", g.name),
                "provider": "openai",
                "model": g.name,
                "apiBase": format!("http://127.0.0.1:{}/v1", config.gateway.port),
            });
            if let Some(key) = master_key {
                m["apiKey"] = serde_json::Value::String(key.to_string());
            }
            m
        })
        .collect();
    json_pretty(&serde_json::json!({ "models": models }))
}

/// Stable step ids.
pub const STEP_DETECT_HARDWARE: &str = "detect-hardware";
pub const STEP_INSTALL_LLAMACPP: &str = "install-llamacpp";
pub const STEP_CONFIGURE_TENSOR_SPLIT: &str = "configure-tensor-split";
pub const STEP_DOWNLOAD_MODELS: &str = "download-models";
pub const STEP_START_GATEWAY: &str = "start-gateway";
pub const STEP_VSCODE_EXTENSIONS: &str = "vscode-extensions";
pub const STEP_MCP_SERVERS: &str = "mcp-servers";
pub const STEP_WINML_REGISTER: &str = "winml-register";
pub const STEP_MXC_POLICIES: &str = "mxc-policies";
pub const STEP_SMOKE_TEST: &str = "smoke-test";

/// Resolve a possibly-relative path against the data dir.
fn resolve_dest(data_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        data_dir.join(p)
    }
}

/// Serialize an already-JSON `serde_json::Value` without panicking.
///
/// Serialization of a `Value` is infallible in practice (no user `Serialize`
/// impls are involved); the debug fallback exists only to honor the
/// no-panic rule — it should never trigger.
fn json_pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| format!("{v:#?}"))
}

/// Build the ten steps in spec order.
pub fn default_steps() -> Vec<Box<dyn WizardStep>> {
    vec![
        Box::new(DetectHardware),
        Box::new(InstallLlamaCpp::with_pin(LlamaCppPin::default_pin())),
        Box::new(ConfigureTensorSplit),
        Box::new(DownloadModels),
        Box::new(StartGateway::default()),
        Box::new(VscodeExtensions::default()),
        Box::new(McpServers),
        Box::new(WinMlRegister),
        Box::new(MxcPolicies),
        Box::new(SmokeTest::default()),
    ]
}

// ---------------------------------------------------------------------------
// 1. Detect hardware and drivers
// ---------------------------------------------------------------------------

/// Step 1: GPU inventory + driver check (§3.3 verification matrix).
pub struct DetectHardware;

#[async_trait]
impl WizardStep for DetectHardware {
    fn id(&self) -> &str {
        STEP_DETECT_HARDWARE
    }
    fn name(&self) -> &str {
        "Detect hardware and drivers"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Exactly what RealSystemOps::gpu_inventory runs, in order: the
        // per-GPU query, then the plain nvidia-smi header for the driver
        // version. The program is manager-telemetry's System32-first
        // resolution (same as the real probe), not a bare PATH lookup.
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        vec![
            PlannedCommand::new(vec![
                smi.clone(),
                "--query-gpu=index,name,uuid,memory.total,compute_cap".to_string(),
                "--format=csv,noheader".to_string(),
            ]),
            PlannedCommand::new(vec![smi]),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("install a current NVIDIA driver (R550+, i.e. 551.61+) and re-run the hardware check — download: https://www.nvidia.com/Download/index.aspx")
    }
    fn help_url(&self) -> Option<&str> {
        Some(crate::NVIDIA_DRIVER_DOWNLOAD_URL)
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let gpus = match ctx.ops.gpu_inventory().await {
            Ok(g) => g,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("GPU probe failed: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        };
        if gpus.is_empty() {
            return Ok(StepOutcome::failed(
                "no NVIDIA GPUs detected",
                vec!["GPU inventory probe returned zero devices".to_string()],
            ));
        }
        let mut log = Vec::new();
        for g in &gpus {
            log.push(format!(
                "gpu{}: {} ({}) — {} MiB VRAM, sm_{}{}, driver {}",
                g.index,
                g.name,
                g.uuid,
                g.total_vram_mib,
                g.compute_capability.0,
                g.compute_capability.1,
                g.driver_version
            ));
        }
        let driver = &gpus[0].driver_version;
        let major: u32 = driver.split('.').next().unwrap_or("0").parse().unwrap_or(0);
        if major < crate::MIN_DRIVER_MAJOR {
            return Ok(StepOutcome::failed(
                format!(
                    "driver {driver} too old: need R{}+ for the pinned CUDA 12.x runtime (spec §3.4)",
                    crate::MIN_DRIVER_MAJOR
                ),
                log,
            ));
        }
        // PSEC/ingress probe result is recorded by the MXC step; here we just
        // note the driver/CUDA line for the report.
        Ok(StepOutcome::done(
            format!(
                "{} GPU(s) detected, driver {driver} (CUDA 12.x runtime compatible)",
                gpus.len()
            ),
            log,
        ))
    }
}

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
    fn archives(&self) -> [LlamaCppAsset; 2] {
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
    fn vcredist_present(system32: &std::path::Path) -> bool {
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
                        log.push(format!("downloading {url}"));
                        // The planned commands must equal the spawned
                        // actions: the downloads in this loop are exactly
                        // planned_commands()[0] and planned_commands()[1],
                        // in this order, when they are needed at all.
                        match ctx.ops.download(&url, &archive, &|_, _| {}).await {
                            Ok(bytes) => log.push(format!("downloaded {bytes} bytes")),
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
                    if let Err(e) = ctx.ops.extract_zip(&archive, &dir).await {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                    }
                    log.push(format!(
                        "extracted {} to {}",
                        archive_pin.asset,
                        dir.display()
                    ));
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

// ---------------------------------------------------------------------------
// 3. Configure tensor-split
// ---------------------------------------------------------------------------

/// Compute per-GPU tensor-split fractions (hundredths, summing to exactly
/// 1.0), proportional to VRAM.
pub fn compute_splits(vram_mib: &[u64]) -> Result<Vec<f32>, WizardError> {
    let total: u64 = vram_mib.iter().sum();
    if total == 0 {
        return Err(WizardError::StepFailed(
            STEP_CONFIGURE_TENSOR_SPLIT.to_string(),
            "zero total VRAM across discovered GPUs".to_string(),
        ));
    }
    // Integer hundredths: exact sum, rounding residue goes to the largest share.
    let mut hundredths: Vec<i64> = vram_mib
        .iter()
        .map(|v| ((*v as f64 / total as f64) * 100.0).round() as i64)
        .collect();
    let residue = 100 - hundredths.iter().sum::<i64>();
    if let Some(idx) = hundredths
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.cmp(b.1))
        .map(|(i, _)| i)
    {
        hundredths[idx] += residue;
    }
    // Defensive: adversarial rounding on very large GPU counts can push a
    // share negative (each value rounds up by < 0.5, so the residue can be
    // as low as -N/2). Clamp negatives to zero and move the deficit onto
    // the positive shares, largest first; the sum stays exactly 100 and no
    // new negatives are created, so this terminates in one pass.
    let deficit: i64 = hundredths.iter().filter(|&&h| h < 0).map(|h| -h).sum();
    if deficit > 0 {
        for h in hundredths.iter_mut() {
            if *h < 0 {
                *h = 0;
            }
        }
        let mut order: Vec<usize> = (0..hundredths.len()).collect();
        order.sort_by(|&a, &b| hundredths[b].cmp(&hundredths[a]));
        let mut remaining = deficit;
        for &i in &order {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(hundredths[i]);
            hundredths[i] -= take;
            remaining -= take;
        }
        debug_assert_eq!(
            remaining, 0,
            "positive shares always cover the deficit (they sum to 100 + deficit)"
        );
    }
    Ok(hundredths.iter().map(|h| *h as f32 / 100.0).collect())
}

/// Step 3: compute per-backend tensor splits from discovered VRAM, write the
/// flag sets, validate sums to 1.0. Audit mode re-validates.
pub struct ConfigureTensorSplit;

impl ConfigureTensorSplit {
    fn splits_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("wizard").join("tensor-split.json")
    }
}

#[async_trait]
impl WizardStep for ConfigureTensorSplit {
    fn id(&self) -> &str {
        STEP_CONFIGURE_TENSOR_SPLIT
    }
    fn name(&self) -> &str {
        "Configure tensor-split"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![
            STEP_DETECT_HARDWARE.to_string(),
            STEP_INSTALL_LLAMACPP.to_string(),
        ]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Pure compute + file write; no external commands.
        vec![]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run configure-tensor-split to recompute splits from current VRAM")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let gpus = match ctx.ops.gpu_inventory().await {
            Ok(g) => g,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("GPU probe failed: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        };
        if gpus.is_empty() {
            return Ok(StepOutcome::failed(
                "no GPUs discovered; cannot compute tensor splits",
                vec![],
            ));
        }
        let path = self.splits_path(ctx);
        match ctx.mode {
            RunMode::Install => {
                let vrams: Vec<u64> = gpus.iter().map(|g| g.total_vram_mib).collect();
                let splits = match compute_splits(&vrams) {
                    Ok(s) => s,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                let sum: f32 = splits.iter().sum();
                debug_assert!((sum - 1.0).abs() < 1e-6);
                let mut map = serde_json::Map::new();
                let mut n = 0;
                for backend in ctx.config.backends.iter().filter(|b| b.enabled) {
                    map.insert(backend.id.clone(), serde_json::json!(splits));
                    n += 1;
                }
                let doc = serde_json::Value::Object(map);
                if let Err(e) = ctx
                    .ops
                    .write_file(&path, json_pretty(&doc).as_bytes())
                    .await
                {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                let split_str = splits
                    .iter()
                    .map(|s| format!("{s:.2}"))
                    .collect::<Vec<_>>()
                    .join("/");
                Ok(StepOutcome::done(
                    format!(
                        "tensor-split [{split_str}] written for {n} enabled backend(s) across {} GPU(s)",
                        gpus.len()
                    ),
                    vec![format!("wrote {}", path.display())],
                ))
            }
            RunMode::Audit => {
                let data = match ctx.ops.read_file(&path).await {
                    Ok(d) => d,
                    Err(_) => {
                        return Ok(StepOutcome::failed(
                            "tensor-split.json missing; re-run configure-tensor-split".to_string(),
                            vec![format!("{} not found", path.display())],
                        ))
                    }
                };
                let doc: serde_json::Value = match serde_json::from_slice(&data) {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("tensor-split.json unparseable: {e}"),
                            vec![],
                        ))
                    }
                };
                let mut log = Vec::new();
                let obj = doc.as_object().cloned().unwrap_or_default();
                for (backend_id, splits) in &obj {
                    let arr: Vec<f32> = splits
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_f64().map(|f| f as f32))
                                .collect()
                        })
                        .unwrap_or_default();
                    let sum: f32 = arr.iter().sum();
                    if arr.len() != gpus.len() {
                        return Ok(StepOutcome::failed(
                            format!(
                                "backend '{backend_id}': split covers {} GPUs, {} discovered",
                                arr.len(),
                                gpus.len()
                            ),
                            log,
                        ));
                    }
                    if (sum - 1.0).abs() > 1e-3 {
                        return Ok(StepOutcome::failed(
                            format!("backend '{backend_id}': splits sum to {sum:.4}, not 1.0"),
                            log,
                        ));
                    }
                    log.push(format!("backend '{backend_id}': splits sum to 1.0 ✓"));
                }
                Ok(StepOutcome::done(
                    format!("tensor-split config valid for {} backend(s)", obj.len()),
                    log,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Download models + checksum verify
// ---------------------------------------------------------------------------

/// Step 4: download curated GGUFs with SHA-256 verification.
/// Audit mode re-verifies installed files without downloading.
///
/// Every model whose checksum matches during a run — downloaded,
/// already present, or audited — gets `verified_at` stamped into the
/// persisted config (see [`DownloadModels::stamp_verified_at`]), so
/// the app's fail-closed backend-start gate accepts wizard-verified
/// models without a second verification in the Models view.
///
/// Download URLs come from [`ModelConfig::source_url`](manager_config::ModelConfig)
/// (the curated catalog, spec §4.6); a model with an empty `source_url` fails
/// closed with a clear message.
pub struct DownloadModels;

#[async_trait]
impl WizardStep for DownloadModels {
    fn id(&self) -> &str {
        STEP_DOWNLOAD_MODELS
    }
    fn name(&self) -> &str {
        "Download models and verify checksums"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_INSTALL_LLAMACPP.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        ctx.config
            .models
            .iter()
            .filter(|m| m.enabled && !m.source_url.is_empty())
            .map(|m| {
                let dest = resolve_dest(&ctx.data_dir, &m.gguf_path);
                PlannedCommand::new(vec![
                    "download".to_string(),
                    m.source_url.clone(),
                    dest.display().to_string(),
                ])
            })
            .collect()
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("model checksum mismatch → re-download?")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        if ctx.config.models.is_empty() {
            return Ok(StepOutcome::done(
                "no models configured; nothing to download",
                vec![],
            ));
        }
        // Disabled models are out of scope for the wizard: their files
        // and configuration stay, but nothing downloads or verifies.
        let targets: Vec<&ModelConfig> = ctx.config.models.iter().filter(|m| m.enabled).collect();
        if targets.is_empty() {
            return Ok(StepOutcome::done(
                "no models enabled; nothing to download (enable models in the Models view)",
                vec![],
            ));
        }
        let mut log = Vec::new();
        let mut verified = 0;
        let mut unverified = 0;
        if ctx.mode == RunMode::Install {
            if let Err(outcome) = self.check_disk_space(ctx, &mut log).await {
                return Ok(outcome);
            }
        }
        for model in &targets {
            let dest = resolve_dest(&ctx.data_dir, &model.gguf_path);
            if model.source_url.is_empty() {
                return Ok(StepOutcome::failed(
                    format!("no source URL configured for model '{}'", model.id),
                    vec!["set models[].source_url in the config (spec §4.6)".to_string()],
                ));
            }
            let url = model.source_url.clone();
            // Only a pinned hash can verify. A model without one (a
            // user-added model with no recorded SHA-256) downloads and
            // runs unverified rather than failing a comparison that can
            // never succeed.
            let pinned = is_pinned_hash(&model.sha256);
            let verify = |actual: &str| pinned && checksums_match(actual, &model.sha256);
            match ctx.mode {
                RunMode::Install => {
                    let mut needs_download = true;
                    if ctx.ops.file_exists(&dest).await {
                        if !pinned {
                            log.push(format!(
                                "{}: already present (no pinned SHA-256; not verified)",
                                model.id
                            ));
                            needs_download = false;
                        } else {
                            match ctx.ops.sha256_file(&dest).await {
                                Ok(h) if verify(&h) => {
                                    log.push(format!("{}: already present and verified", model.id));
                                    needs_download = false;
                                }
                                _ => log.push(format!(
                                    "{}: present but checksum mismatch; re-downloading",
                                    model.id
                                )),
                            }
                        }
                    }
                    if needs_download {
                        log.push(format!("downloading {} → {}", url, dest.display()));
                        if let Err(e) = ctx.ops.download(&url, &dest, &|_, _| {}).await {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                        }
                        if pinned {
                            let actual = match ctx.ops.sha256_file(&dest).await {
                                Ok(h) => h,
                                Err(e) => {
                                    return Ok(StepOutcome::failed(
                                        format!("{e}"),
                                        vec![format!("{e:?}")],
                                    ))
                                }
                            };
                            if !verify(&actual) {
                                return Ok(StepOutcome::failed(
                                    format!(
                                        "checksum mismatch for '{}': expected {}, got {}",
                                        model.id, model.sha256, actual
                                    ),
                                    vec!["downloaded file rejected".to_string()],
                                ));
                            }
                            log.push(format!("{}: downloaded and SHA-256 verified", model.id));
                        } else {
                            log.push(format!(
                                "{}: downloaded (no pinned SHA-256; not verified)",
                                model.id
                            ));
                        }
                    }
                    if pinned {
                        Self::stamp_verified_at(ctx, &model.id, &mut log);
                        verified += 1;
                    } else {
                        unverified += 1;
                    }
                }
                RunMode::Audit => {
                    if !ctx.ops.file_exists(&dest).await {
                        return Ok(StepOutcome::failed(
                            format!("model '{}' missing at {}", model.id, dest.display()),
                            vec![],
                        ));
                    }
                    if !pinned {
                        log.push(format!(
                            "{}: present (no pinned SHA-256; cannot verify)",
                            model.id
                        ));
                        unverified += 1;
                        continue;
                    }
                    match ctx.ops.sha256_file(&dest).await {
                        Ok(h) if verify(&h) => {
                            log.push(format!("{}: checksum verified", model.id));
                            Self::stamp_verified_at(ctx, &model.id, &mut log);
                            verified += 1;
                        }
                        Ok(h) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "model '{}' checksum mismatch: expected {}, got {}",
                                    model.id, model.sha256, h
                                ),
                                log,
                            ));
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(
                                format!("cannot read '{}': {e}", model.id),
                                vec![format!("{e:?}")],
                            ));
                        }
                    }
                }
            }
        }
        let mut message = format!("{verified}/{} model(s) verified", targets.len());
        if unverified > 0 {
            message.push_str(&format!(
                "; {unverified} present without a pinned SHA-256 (not verified)"
            ));
        }
        Ok(StepOutcome::done(message, log))
    }
}

impl DownloadModels {
    /// Stamp `verified_at` for one model this run just verified into
    /// the persisted config.
    ///
    /// Steps receive a snapshot clone of the app config
    /// (`WizardContext::config` is built fresh by the caller and steps
    /// run through `&WizardContext`), so the stamp cannot ride back
    /// through `ctx.config` — nothing a step does to it reaches the
    /// saved `AppConfig`. The persisted config lives at
    /// `<data_dir>/config.json` (the path the app loads at startup);
    /// it is the base when it loads. On a first run, when nothing has
    /// saved that file yet, the context config is the base instead —
    /// it is the caller's current config, cloned for this step run,
    /// so saving it back persists exactly what the app would save
    /// later, plus the stamp. When there is no real data dir on disk
    /// (unit tests drive the step with fake ops and a fictional data
    /// dir) there is no persisted config to stamp and this is a no-op.
    ///
    /// Best-effort: failures are logged, never failed into the step
    /// outcome. The verification itself already succeeded; an
    /// unstamped model simply stays gated until something else
    /// verifies it.
    fn stamp_verified_at(ctx: &WizardContext, model_id: &str, log: &mut Vec<String>) {
        let config_path = ctx.data_dir.join("config.json");
        let mut config = match manager_config::load(&config_path) {
            Ok(cfg) => cfg,
            Err(_) if ctx.data_dir.is_dir() => ctx.config.clone(),
            Err(_) => return,
        };
        let Some(model) = config.models.iter_mut().find(|m| m.id == model_id) else {
            return;
        };
        model.verified_at = Some(chrono::Utc::now());
        match manager_config::save(&config, &config_path) {
            Ok(()) => log.push(format!(
                "{model_id}: verified_at stamped in {}",
                config_path.display()
            )),
            Err(e) => log.push(format!(
                "{model_id}: could not stamp verified_at in {}: {e}",
                config_path.display()
            )),
        }
    }

    /// Fail when the models still needing download exceed the free space on
    /// the data volume (10% headroom). Sizes come from HEAD Content-Length;
    /// models whose size cannot be determined are logged, not counted. A
    /// failed free-space probe warns instead of blocking.
    async fn check_disk_space(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let mut known: u64 = 0;
        let mut unknown: u32 = 0;
        for model in &ctx.config.models {
            if !model.enabled || model.source_url.is_empty() {
                continue; // disabled models are out of scope; the per-model loop reports a missing URL
            }
            let dest = resolve_dest(&ctx.data_dir, &model.gguf_path);
            let needs_download = if ctx.ops.file_exists(&dest).await {
                // An unpinned model already on disk cannot be verified,
                // so it counts as present — same rule as the run loop.
                is_pinned_hash(&model.sha256)
                    && !matches!(ctx.ops.sha256_file(&dest).await, Ok(h) if checksums_match(&h, &model.sha256))
            } else {
                true
            };
            if !needs_download {
                continue;
            }
            match ctx.ops.http_head_content_length(&model.source_url).await {
                Ok(Some(n)) => {
                    known = known.saturating_add(n);
                    log.push(format!("{}: download size ~{n} bytes", model.id));
                }
                _ => {
                    unknown += 1;
                    log.push(format!(
                        "{}: size unknown, excluded from space check",
                        model.id
                    ));
                }
            }
        }
        if known == 0 {
            log.push(format!(
                "disk-space pre-check skipped: no download sizes known ({unknown} model(s) with unknown size)"
            ));
            return Ok(());
        }
        let free = match ctx.ops.free_space_bytes(&ctx.data_dir).await {
            Ok(f) => f,
            Err(e) => {
                log.push(format!(
                    "free-space probe failed ({e}); skipping space check"
                ));
                return Ok(());
            }
        };
        let needed = known + known / 10; // 10% headroom for temp files
        log.push(format!(
            "space check: need ~{needed} bytes, {free} bytes free on {}",
            ctx.data_dir.display()
        ));
        if free < needed {
            return Err(StepOutcome::failed(
                format!(
                    "insufficient disk space: need ~{:.1} GB for model downloads, have {:.1} GB free on {}",
                    needed as f64 / 1e9,
                    free as f64 / 1e9,
                    ctx.data_dir.display()
                ),
                vec![
                    "free up disk space or point data_dir at a larger volume, then re-run".to_string(),
                ],
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 5. Start LiteLLM gateway
// ---------------------------------------------------------------------------

/// Step 5: bootstrap the managed Python env (uv), generate `config.yaml`
/// from the app config, spawn LiteLLM on :4000, health-check.
pub struct StartGateway {
    pub health_retries: u32,
    pub health_interval_ms: u64,
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
        }
    }
}

impl StartGateway {
    fn venv_dir(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("python").join("venv")
    }

    /// Directory holding the bootstrapped uv binary.
    fn uv_bin_dir(ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("bin")
    }

    fn uv_binary_name() -> &'static str {
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
    fn find_uv_in(dirs: &[PathBuf]) -> Option<PathBuf> {
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
    fn uv_argv0(ctx: &WizardContext) -> (PathBuf, bool) {
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

    fn uv_archive_name() -> String {
        format!("uv-{}.zip", crate::UV_PINNED_VERSION)
    }

    /// Resolve a usable `uv`: PATH first, then the bootstrapped copy under
    /// data_dir/bin, else download + extract the pinned standalone build
    /// (Windows only; other platforms fail with install guidance).
    async fn ensure_uv(&self, ctx: &WizardContext) -> Result<PathBuf, StepOutcome> {
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
        ctx.ops
            .download(&url, &archive, &|_, _| {})
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!("uv bootstrap download failed: {e}"),
                    vec![format!("{e:?}")],
                )
            })?;
        ctx.ops.extract_zip(&archive, &dir).await.map_err(|e| {
            StepOutcome::failed(
                format!("uv bootstrap extraction failed: {e}"),
                vec![format!("{e:?}")],
            )
        })?;
        Ok(bundled)
    }

    fn venv_python(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }

    fn gateway_url(&self, ctx: &WizardContext) -> String {
        format!("http://127.0.0.1:{}/v1/models", ctx.config.gateway.port)
    }

    fn litellm_spec(&self, ctx: &WizardContext) -> String {
        match ctx
            .config
            .updates
            .components
            .get("litellm")
            .and_then(|c| c.pinned_version.as_deref())
        {
            Some(v) => format!("litellm[proxy]=={v}"),
            None => "litellm[proxy]".to_string(),
        }
    }

    fn venv_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
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
    fn version_probe_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')".to_string(),
        ])
    }

    /// Probe that succeeds only when litellm imports in the venv.
    fn litellm_probe_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            // prisma rides along: a venv that predates its addition
            // to the install spec (see pip_command) must not pass
            // this probe, so the pip step runs and completes it.
            "import litellm, prisma".to_string(),
        ])
    }

    /// The existing venv interpreter's "major.minor" version, or None
    /// when there is no venv interpreter or it cannot report one.
    async fn venv_python_version(&self, ctx: &WizardContext) -> Option<String> {
        let python = self.venv_python(ctx);
        if !ctx.ops.file_exists(&python).await {
            return None;
        }
        let out = ctx
            .ops
            .run_command(&self.version_probe_command(ctx))
            .await
            .ok()?;
        if !out.success() {
            return None;
        }
        let v = out.stdout.trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    /// Whether litellm already imports in the venv.
    async fn litellm_importable(&self, ctx: &WizardContext) -> bool {
        matches!(
            ctx.ops.run_command(&self.litellm_probe_command(ctx)).await,
            Ok(out) if out.success()
        )
    }

    fn pip_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
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
            // clean 401. Found on real hardware 2026-10-15.
            "prisma".to_string(),
        ])
    }

    /// The venv's litellm console script. LiteLLM cannot be run as
    /// `python -m litellm` — the package ships no `__main__` and exits
    /// immediately with "No module named litellm.__main__" (found on
    /// real hardware 2026-10-15; the detached spawn discarded the
    /// output, so the failure surfaced only as a health-check
    /// timeout). The gateway must be spawned through the `litellm`
    /// entry point the installer lays down in the venv.
    fn litellm_cli(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(windows) {
            venv.join("Scripts").join("litellm.exe")
        } else {
            venv.join("bin").join("litellm")
        }
    }

    fn spawn_command(&self, ctx: &WizardContext) -> PlannedCommand {
        let cfg = ctx.data_dir.join("gateway").join("config.yaml");
        let mut cmd = PlannedCommand::new(vec![
            self.litellm_cli(ctx).display().to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
            // Loopback only: without --host LiteLLM binds 0.0.0.0 and
            // exposes the gateway (and its upstream keys) to the LAN.
            "--host".to_string(),
            "127.0.0.1".to_string(),
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
    async fn gateway_auth_headers(&self, ctx: &WizardContext) -> Vec<(String, String)> {
        match self.stored_master_key(ctx).await {
            Some(key) => vec![("Authorization".to_string(), format!("Bearer {key}"))],
            None => Vec::new(),
        }
    }

    async fn spawn_command_with_env(
        &self,
        ctx: &WizardContext,
    ) -> Result<(PlannedCommand, Option<String>), StepOutcome> {
        let mut cmd = self.spawn_command(ctx);
        // The master key is unconditional: current LiteLLM refuses
        // to boot without one. Failing to obtain it fails the step
        // here rather than spawning a gateway that exits at startup.
        let master_key = self.ensure_master_key(ctx).await?;
        cmd.env.insert("LITELLM_MASTER_KEY".to_string(), master_key);
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

    /// Stop the gateway recorded in `<data_dir>/gateway.pid`, if any.
    ///
    /// Restart semantics: the previously spawned LiteLLM still holds
    /// the port and serves the old generated config, so spawning a
    /// second one would fail to bind (silently — its stdio is nulled)
    /// while the health check passes against the old process. Stopping
    /// the recorded pid first is what makes re-running this step a
    /// real restart. A live recorded process that cannot be stopped
    /// fails the step: failing loudly beats a duplicate spawn.
    async fn stop_previous_gateway(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let pid_path = crate::gateway_pid_path(&ctx.data_dir);
        let recorded = match ctx.ops.read_file(&pid_path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).trim().parse::<u32>().ok(),
            Err(_) => None,
        };
        let Some(old_pid) = recorded else {
            return Ok(());
        };
        if ctx.ops.process_alive(old_pid) {
            if let Err(e) = ctx.ops.kill_process(old_pid) {
                return Err(StepOutcome::failed(
                    format!(
                        "could not stop the previous gateway (pid {old_pid}): {e}; \
                         refusing to spawn a duplicate on :{}",
                        ctx.config.gateway.port
                    ),
                    log.clone(),
                ));
            }
            log.push(format!("stopped previous gateway (pid {old_pid})"));
            // Wait briefly for the process to actually exit so the
            // port is free before the replacement spawns.
            for _ in 0..30 {
                if !ctx.ops.process_alive(old_pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if ctx.ops.process_alive(old_pid) {
                return Err(StepOutcome::failed(
                    format!(
                        "previous gateway (pid {old_pid}) is still running after a stop \
                         request; refusing to spawn a duplicate on :{}",
                        ctx.config.gateway.port
                    ),
                    log.clone(),
                ));
            }
        }
        let _ = ctx.ops.remove_file(&pid_path).await;
        Ok(())
    }

    /// Files the gateway child's stdout/stderr are captured to.
    fn gateway_stdout_log(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("logs").join("gateway.stdout.log")
    }

    fn gateway_stderr_log(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("logs").join("gateway.stderr.log")
    }

    /// Append the tail of the captured gateway logs to the step log,
    /// so a startup failure shows the process's own error output in
    /// the wizard instead of only a probe timeout.
    async fn append_gateway_log_tail(&self, ctx: &WizardContext, log: &mut Vec<String>) {
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

    async fn wait_healthy(&self, ctx: &WizardContext, pid: u32, log: &mut Vec<String>) -> bool {
        let url = self.gateway_url(ctx);
        let headers = self.gateway_auth_headers(ctx).await;
        for attempt in 1..=self.health_retries {
            // Fail fast when the child is gone: probing a dead process
            // for the full budget reports a timeout and hides the real
            // failure, which the captured log tail then has to explain.
            if !ctx.ops.process_alive(pid) {
                log.push(format!(
                    "gateway process (pid {pid}) exited during startup (probe attempt {attempt})"
                ));
                return false;
            }
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    log.push(format!(
                        "gateway healthy on :{} (attempt {attempt})",
                        ctx.config.gateway.port
                    ));
                    return true;
                }
                Ok(resp) => log.push(format!(
                    "health probe attempt {attempt}: HTTP {}",
                    resp.status
                )),
                Err(e) => log.push(format!("health probe attempt {attempt}: {e}")),
            }
            if attempt < self.health_retries {
                tokio::time::sleep(std::time::Duration::from_millis(self.health_interval_ms)).await;
            }
        }
        false
    }
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
            // bootstrap download when uv is not yet resolvable). The two
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
                cmds.push(spawn);
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
        let mut log = Vec::new();
        match ctx.mode {
            RunMode::Install => {
                let uv = match self.ensure_uv(ctx).await {
                    Ok(p) => {
                        log.push(format!("using uv at {}", p.display()));
                        p
                    }
                    Err(outcome) => return Ok(outcome),
                };
                // Reuse a healthy existing venv: one already on the
                // pinned Python with litellm importable needs no venv
                // creation and no package install. Anything else is
                // (re)created — `uv venv --clear` replaces a venv left
                // behind by a failed install or built with an unpinned
                // (too new) Python, which previously made every retry
                // fail with "already exists" before the real error.
                let existing_version = self.venv_python_version(ctx).await;
                let venv_current =
                    existing_version.as_deref() == Some(crate::GATEWAY_PYTHON_VERSION);
                let reuse = venv_current && self.litellm_importable(ctx).await;
                if reuse {
                    log.push(format!(
                        "reusing existing venv (Python {}) — litellm already installed",
                        crate::GATEWAY_PYTHON_VERSION
                    ));
                } else if !venv_current {
                    match &existing_version {
                        Some(v) => log.push(format!(
                            "existing venv uses Python {v}; recreating it with Python {}",
                            crate::GATEWAY_PYTHON_VERSION
                        )),
                        None => log.push(format!(
                            "creating venv with Python {}",
                            crate::GATEWAY_PYTHON_VERSION
                        )),
                    }
                }
                let setup_cmds: Vec<PlannedCommand> = if reuse {
                    Vec::new()
                } else {
                    vec![self.venv_command(ctx, &uv), self.pip_command(ctx, &uv)]
                };
                for cmd in &setup_cmds {
                    log.push(format!("running: {}", cmd.argv.join(" ")));
                    match ctx.ops.run_command(cmd).await {
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
                                vec![out.stderr],
                            ))
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                }
                // config.yaml is generated from app state, never hand-edited.
                let yaml = match manager_gateway::generate_config_yaml(&ctx.config) {
                    Ok(y) => y,
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("gateway config generation failed: {e}"),
                            vec![format!("{e:?}")],
                        ))
                    }
                };
                let cfg_path = ctx.data_dir.join("gateway").join("config.yaml");
                if let Err(e) = ctx.ops.write_file(&cfg_path, yaml.as_bytes()).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(format!("wrote {}", cfg_path.display()));
                // Restart = stop the recorded gateway first; without
                // this the spawn below is a duplicate that fails to
                // bind while the old process keeps serving.
                if let Err(outcome) = self.stop_previous_gateway(ctx, &mut log).await {
                    return Ok(outcome);
                }
                let (mut spawn, openrouter_warning) = match self.spawn_command_with_env(ctx).await {
                    Ok(pair) => pair,
                    Err(outcome) => return Ok(outcome),
                };
                if let Some(w) = &openrouter_warning {
                    log.push(format!("warning: {w}"));
                }
                // Capture the gateway's own output: without it a
                // startup failure is undiagnosable from inside the app.
                spawn.stdout_log = Some(self.gateway_stdout_log(ctx));
                spawn.stderr_log = Some(self.gateway_stderr_log(ctx));
                log.push(format!(
                    "gateway output is captured to {} and {}",
                    self.gateway_stdout_log(ctx).display(),
                    self.gateway_stderr_log(ctx).display()
                ));
                let pid = match ctx.ops.spawn_detached(&spawn).await {
                    Ok(pid) => pid,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                log.push(format!("LiteLLM spawned (pid {pid})"));
                // Record the pid so a later stop (the gateway_stop
                // command) or restart (this step re-run) terminates
                // exactly this process instead of guessing by port.
                let pid_path = crate::gateway_pid_path(&ctx.data_dir);
                match ctx
                    .ops
                    .write_file(&pid_path, pid.to_string().as_bytes())
                    .await
                {
                    Ok(()) => log.push(format!("recorded gateway pid in {}", pid_path.display())),
                    Err(e) => log.push(format!(
                        "warning: could not record the gateway pid in {}: {e}; \
                         stopping this gateway later will not be possible",
                        pid_path.display()
                    )),
                }
                if !self.wait_healthy(ctx, pid, &mut log).await {
                    self.append_gateway_log_tail(ctx, &mut log).await;
                    return Ok(StepOutcome::failed(
                        format!(
                            "gateway did not become healthy on :{} — the gateway log tail follows; full logs in {}",
                            ctx.config.gateway.port,
                            ctx.data_dir.join("logs").display()
                        ),
                        log,
                    ));
                }
                let message = match &openrouter_warning {
                    Some(w) => format!(
                        "LiteLLM gateway running on :{} — warning: {w}",
                        ctx.config.gateway.port
                    ),
                    None => format!("LiteLLM gateway running on :{}", ctx.config.gateway.port),
                };
                Ok(StepOutcome::done(message, log))
            }
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

// ---------------------------------------------------------------------------
// 6. Install VS Code extensions
// ---------------------------------------------------------------------------

/// Step 6: install Continue.dev + Cline, write the Continue config pointing
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
            ],
        }
    }
}

impl VscodeExtensions {
    /// Generate a Continue config with every gateway group as a model,
    /// carrying the gateway master key as each model's apiKey.
    async fn continue_config_json(&self, ctx: &WizardContext) -> Result<String, StepOutcome> {
        let key = crate::ensure_gateway_master_key(ctx.ops.as_ref())
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!("could not obtain the gateway master key for the editor config: {e}"),
                    vec![format!("{e:?}")],
                )
            })?;
        Ok(continue_editor_config(&ctx.config, Some(&key)))
    }

    fn install_commands(&self) -> Vec<PlannedCommand> {
        self.extensions
            .iter()
            .map(|ext| {
                PlannedCommand::new(vec![
                    "code".to_string(),
                    "--install-extension".to_string(),
                    ext.clone(),
                    "--force".to_string(),
                ])
            })
            .collect()
    }

    /// True when a command error is the ops layer's spawn-level
    /// failure (the executable could not be started at all), as opposed
    /// to a program that ran and exited nonzero. The ops layer reports
    /// spawn failures with exit code -1 and its own `spawn failed:`
    /// prefix.
    fn spawn_failed(e: &WizardError) -> bool {
        matches!(
            e,
            WizardError::CommandFailed { code: -1, stderr, .. }
                if stderr.starts_with("spawn failed")
        )
    }

    /// The skip outcome for a machine without VS Code: the extensions
    /// are optional editor tooling, so a missing `code` executable is
    /// a skip, never a setup failure.
    fn vscode_missing() -> StepOutcome {
        StepOutcome::skipped(
            "VS Code is not installed (the `code` command was not found); extension install skipped",
        )
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
            RunMode::Install => self.install_commands(),
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
        match ctx.mode {
            RunMode::Install => {
                // Probe first: without the `code` CLI there is nothing
                // to install into, and the step skips instead of
                // failing on the first extension.
                let probe = PlannedCommand::new(vec!["code".to_string(), "--version".to_string()]);
                match ctx.ops.run_command(&probe).await {
                    Ok(_) => {}
                    Err(e) if Self::spawn_failed(&e) => return Ok(Self::vscode_missing()),
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                for cmd in self.install_commands() {
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
                        Err(e) if Self::spawn_failed(&e) => return Ok(Self::vscode_missing()),
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                }
                let config_json = match self.continue_config_json(ctx).await {
                    Ok(json) => json,
                    Err(outcome) => return Ok(outcome),
                };
                // Never clobber an existing Continue config: write ours to the
                // data dir always, and to the canonical location only when
                // nothing is there yet.
                let staged = ctx.data_dir.join("vscode").join("continue-config.json");
                if let Err(e) = ctx.ops.write_file(&staged, config_json.as_bytes()).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(format!("staged Continue config at {}", staged.display()));
                match ctx.ops.home_dir() {
                    Some(home) => {
                        let canonical = home.join(".continue").join("config.json");
                        if ctx.ops.file_exists(&canonical).await {
                            let sidecar = home.join(".continue").join("config.json.llm-manager");
                            if let Err(e) =
                                ctx.ops.write_file(&sidecar, config_json.as_bytes()).await
                            {
                                return Ok(StepOutcome::failed(
                                    format!("{e}"),
                                    vec![format!("{e:?}")],
                                ));
                            }
                            log.push(format!(
                                "existing Continue config left untouched; generated config at {}",
                                sidecar.display()
                            ));
                        } else if let Err(e) =
                            ctx.ops.write_file(&canonical, config_json.as_bytes()).await
                        {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                        } else {
                            log.push(format!("wrote {}", canonical.display()));
                        }
                    }
                    None => log.push("home directory unknown; config staged only".to_string()),
                }
                Ok(StepOutcome::done(
                    format!("{} VS Code extension(s) installed", self.extensions.len()),
                    log,
                ))
            }
            RunMode::Audit => {
                let cmd =
                    PlannedCommand::new(vec!["code".to_string(), "--list-extensions".to_string()]);
                let out = match ctx.ops.run_command(&cmd).await {
                    Ok(o) => o,
                    Err(e) if Self::spawn_failed(&e) => return Ok(Self::vscode_missing()),
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
                if missing.is_empty() {
                    Ok(StepOutcome::done(
                        format!("{} extension(s) present", self.extensions.len()),
                        vec![],
                    ))
                } else {
                    Ok(StepOutcome::failed(
                        format!("missing VS Code extensions: {}", missing.join(", ")),
                        vec![],
                    ))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Configure MCP servers
// ---------------------------------------------------------------------------

/// Credential Manager targets. Values are never read — presence only.
pub const GITHUB_CRED_TARGET: &str = "local-llm-service-manager/github-mcp";
pub const GDRIVE_CRED_TARGET: &str = "local-llm-service-manager/gdrive-mcp";

/// Step 7: write the MCP server config (GitHub + Google Drive) with
/// credential *references*; verify the credentials exist.
pub struct McpServers;

impl McpServers {
    fn config_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mcp").join("mcp-servers.json")
    }

    fn config_json() -> String {
        json_pretty(&serde_json::json!({
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    // Credential Manager target reference — never a value.
                    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": format!("<credential-manager:{GITHUB_CRED_TARGET}>") }
                },
                "gdrive": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-gdrive"],
                    "env": { "GOOGLE_APPLICATION_CREDENTIALS": format!("<credential-manager:{GDRIVE_CRED_TARGET}>") }
                }
            }
        }))
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
        Some("add the missing credential to Windows Credential Manager, then re-run mcp-servers")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Credential presence check first: values are never shown.
        for target in [GITHUB_CRED_TARGET, GDRIVE_CRED_TARGET] {
            match ctx.ops.credential_exists(target).await {
                Ok(true) => log.push(format!("credential '{target}' present")),
                Ok(false) => {
                    return Ok(StepOutcome::failed(
                        format!("credential '{target}' not found in Windows Credential Manager"),
                        vec![
                            "add the credential, then retry this step".to_string(),
                            "credential values are never displayed or stored".to_string(),
                        ],
                    ))
                }
                Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
            }
        }
        let path = self.config_path(ctx);
        let json = Self::config_json();
        if let Err(e) = ctx.ops.write_file(&path, json.as_bytes()).await {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("wrote {}", path.display()));
        Ok(StepOutcome::done(
            "MCP servers configured (github, gdrive)".to_string(),
            log,
        ))
    }
}

// ---------------------------------------------------------------------------
// 8. Register Windows ML backend
// ---------------------------------------------------------------------------

/// Step 8: probe WinMLServer, validate the model class, record the
/// registration with the researched limits verbatim.
pub struct WinMlRegister;

impl WinMlRegister {
    fn registration_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("winml").join("registration.json")
    }

    fn health_url(&self, ctx: &WizardContext) -> String {
        format!("http://127.0.0.1:{}/v1/models", ctx.config.winml.port)
    }
}

#[async_trait]
impl WizardStep for WinMlRegister {
    fn id(&self) -> &str {
        STEP_WINML_REGISTER
    }
    fn name(&self) -> &str {
        "Register Windows ML backend"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // The probe is an HTTP GET, surfaced here for transparency.
        vec![PlannedCommand::new(vec![
            "http-get".to_string(),
            self.health_url(ctx),
        ])]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("start WinMLServer, then re-run winml-register")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        if !ctx.config.winml.enabled {
            return Ok(StepOutcome::skipped(
                "Windows ML backend disabled in config",
            ));
        }
        let model_id = match &ctx.config.winml.model_id {
            Some(m) => m.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "winml.model_id not configured",
                    vec!["set winml.model_id to a tool-runner-class model".to_string()],
                ))
            }
        };
        // Authoritative eligibility check lives in manager-winml.
        if let Err(e) = manager_winml::model_class_eligible(&model_id) {
            return Ok(StepOutcome::failed(
                format!("model '{model_id}' ineligible for Windows ML: {e}"),
                vec![format!("{e:?}")],
            ));
        }
        let mut log = vec![format!("model '{model_id}' eligible (tool-runner class)")];
        match ctx.ops.http_get(&self.health_url(ctx)).await {
            Ok(resp) if resp.status == 200 => {
                log.push(format!(
                    "WinMLServer responding on :{}",
                    ctx.config.winml.port
                ));
            }
            Ok(resp) => {
                return Ok(StepOutcome::failed(
                    format!("WinMLServer unhealthy: HTTP {}", resp.status),
                    log,
                ))
            }
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!(
                        "WinMLServer not reachable on :{}: {e}",
                        ctx.config.winml.port
                    ),
                    log,
                ))
            }
        }
        // Record the registration with the researched limits verbatim.
        let limits = manager_winml::WinMlLimits::known_limits();
        let reg = serde_json::json!({
            "model_id": model_id,
            "port": ctx.config.winml.port,
            "endpoint": self.health_url(ctx),
            "limits": limits,
            "registered_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = self.registration_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(&path, json_pretty(&reg).as_bytes())
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("wrote {}", path.display()));
        for stmt in manager_winml::WinMlLimits::statements() {
            log.push(format!("limit: {stmt}"));
        }
        Ok(StepOutcome::done(
            format!(
                "Windows ML backend registered: {model_id} on :{} (experimental, no tensor-split)",
                ctx.config.winml.port
            ),
            log,
        ))
    }
}

// ---------------------------------------------------------------------------
// 9. Apply MXC policies
// ---------------------------------------------------------------------------

/// Step 9: probe MXC, write the default policy JSON, validate it,
/// run a harmless sandboxed self-test.
pub struct MxcPolicies;

impl MxcPolicies {
    fn policy_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mxc").join("policy.json")
    }

    fn probe_command() -> PlannedCommand {
        PlannedCommand::new(vec!["wxc-exec.exe".to_string(), "--probe-json".to_string()])
    }

    fn self_test_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "wxc-exec.exe".to_string(),
            "--policy".to_string(),
            self.policy_path(ctx).display().to_string(),
            "--self-test".to_string(),
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
        match ctx.mode {
            RunMode::Install => vec![Self::probe_command(), self.self_test_command(ctx)],
            RunMode::Audit => vec![Self::probe_command()],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("verify the MXC runtime is installed, then re-run mxc-policies")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Probe PSEC/ingress availability.
        match ctx.ops.run_command(&Self::probe_command()).await {
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
                match ctx.ops.run_command(&self.self_test_command(ctx)).await {
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

// ---------------------------------------------------------------------------
// 10. Smoke-test inference
// ---------------------------------------------------------------------------

/// Sanity bound for a smoke-test request (non-streaming proxy for TTFT).
pub const SMOKE_MAX_LATENCY_MS: u64 = 120_000;

/// Step 10: tagged test request per gateway group through the gateway;
/// assert non-empty completion + sane latency; record baselines.
pub struct SmokeTest {
    pub max_latency_ms: u64,
}

impl Default for SmokeTest {
    fn default() -> Self {
        Self {
            max_latency_ms: SMOKE_MAX_LATENCY_MS,
        }
    }
}

impl SmokeTest {
    fn baselines_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("wizard").join("smoke-baselines.json")
    }
}

#[async_trait]
impl WizardStep for SmokeTest {
    fn id(&self) -> &str {
        STEP_SMOKE_TEST
    }
    fn name(&self) -> &str {
        "Smoke-test inference"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // The probes are HTTP POSTs, surfaced here for transparency.
        ctx.config
            .gateway
            .groups
            .iter()
            .map(|g| {
                PlannedCommand::new(vec![
                    "http-post".to_string(),
                    format!(
                        "http://127.0.0.1:{}/v1/chat/completions",
                        ctx.config.gateway.port
                    ),
                    format!("{{\"model\":\"{}\"}}", g.name),
                ])
            })
            .collect()
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("check backend health on the dashboard, then re-run smoke-test")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let groups: Vec<String> = ctx
            .config
            .gateway
            .groups
            .iter()
            .map(|g| g.name.clone())
            .collect();
        if groups.is_empty() {
            return Ok(StepOutcome::failed(
                "no gateway model groups configured",
                vec![],
            ));
        }
        let tag = format!("wizard-{}", chrono::Utc::now().timestamp());
        let url = format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            ctx.config.gateway.port
        );
        // The gateway requires its master key when one is set; it is
        // read from Credential Manager, never generated here.
        let headers: Vec<(String, String)> = match ctx
            .ops
            .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
        {
            Some(key) if !key.is_empty() => {
                vec![("Authorization".to_string(), format!("Bearer {key}"))]
            }
            _ => Vec::new(),
        };
        let mut log = Vec::new();
        let mut baselines = Vec::new();
        for group in &groups {
            let body = serde_json::json!({
                "model": group,
                "messages": [{ "role": "user", "content": format!("wizard smoke test {tag}") }],
                "max_tokens": 8,
                "stream": false,
            })
            .to_string();
            let resp = match ctx.ops.http_post_with_headers(&url, &body, &headers).await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(StepOutcome::failed(
                        format!("smoke test for '{group}' failed: {e}"),
                        vec![format!("{e:?}")],
                    ))
                }
            };
            if resp.status != 200 {
                return Ok(StepOutcome::failed(
                    format!("smoke test for '{group}': HTTP {}", resp.status),
                    vec![resp.body_str()],
                ));
            }
            let v: serde_json::Value = match serde_json::from_slice(&resp.body) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(StepOutcome::failed(
                        format!("unparseable completion for '{group}': {e}"),
                        log,
                    ))
                }
            };
            let content = v
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if content.is_empty() {
                return Ok(StepOutcome::failed(
                    format!("smoke test for '{group}': empty completion"),
                    log,
                ));
            }
            if resp.elapsed_ms > self.max_latency_ms {
                return Ok(StepOutcome::failed(
                    format!(
                        "smoke test for '{group}': latency {} ms exceeded sanity bound {} ms",
                        resp.elapsed_ms, self.max_latency_ms
                    ),
                    log,
                ));
            }
            log.push(format!(
                "{group}: {} ms, {} chars",
                resp.elapsed_ms,
                content.chars().count()
            ));
            baselines.push(serde_json::json!({
                "group": group,
                "latency_ms": resp.elapsed_ms,
                "chars": content.chars().count(),
            }));
        }
        let doc = serde_json::json!({
            "tag": tag,
            "baselines": baselines,
            "recorded_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = self.baselines_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(&path, json_pretty(&doc).as_bytes())
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("baselines written to {}", path.display()));
        Ok(StepOutcome::done(
            format!("smoke test passed for {} group(s)", groups.len()),
            log,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSystemOps;
    use crate::{GpuDescriptor, RunMode, StepState, WizardContext};
    use manager_config::{AppConfig, ComponentUpdate, ModelConfig};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    fn test_gpus() -> Vec<GpuDescriptor> {
        vec![
            GpuDescriptor {
                index: 0,
                name: "NVIDIA RTX A4000".to_string(),
                uuid: "GPU-aaaa".to_string(),
                total_vram_mib: 16384,
                compute_capability: (8, 6),
                driver_version: "581.57".to_string(),
            },
            GpuDescriptor {
                index: 1,
                name: "Tesla P100-PCIE-16GB".to_string(),
                uuid: "GPU-bbbb".to_string(),
                total_vram_mib: 16384,
                compute_capability: (6, 0),
                driver_version: "581.57".to_string(),
            },
            GpuDescriptor {
                index: 2,
                name: "Tesla P100-PCIE-16GB".to_string(),
                uuid: "GPU-cccc".to_string(),
                total_vram_mib: 16384,
                compute_capability: (6, 0),
                driver_version: "581.57".to_string(),
            },
        ]
    }

    fn test_pin() -> LlamaCppPin {
        LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"fake-zip-bytes"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: sha256_hex(b"fake-cudart-bytes"),
            },
        }
    }

    /// Script both archive downloads for `pin` under their real URLs.
    fn script_pin_downloads(ops: &FakeSystemOps, pin: &LlamaCppPin) {
        ops.with_download(&pin.download_url(), b"fake-zip-bytes".to_vec());
        ops.with_download(&pin.runtime_download_url(), b"fake-cudart-bytes".to_vec());
    }

    fn test_config_with_models() -> AppConfig {
        let mut c = AppConfig::default_config();
        for b in c.backends.iter_mut() {
            b.enabled = true;
        }
        c.models = vec![
            ModelConfig {
                id: "qwen3-8b".to_string(),
                gguf_path: "models/qwen3-8b.gguf".into(),
                quant: "Q4_K_M".to_string(),
                params_b: 8.0,
                sha256: sha256_hex(b"gguf-8b-bytes"),
                verified_at: None,
                source_url: "https://example.com/qwen3-8b.gguf".to_string(),
                assigned_backends: vec!["tool-runner".to_string()],
                enabled: true,
            },
            ModelConfig {
                id: "qwen2.5-coder-7b".to_string(),
                gguf_path: "models/qwen2.5-coder-7b.gguf".into(),
                quant: "Q4_K_M".to_string(),
                params_b: 7.0,
                sha256: sha256_hex(b"gguf-7b-bytes"),
                verified_at: None,
                source_url: "https://example.com/qwen2.5-coder-7b.gguf".to_string(),
                assigned_backends: vec!["coder-fast".to_string()],
                enabled: true,
            },
        ];
        c
    }

    fn ctx_with(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
        WizardContext::new(test_config_with_models(), PathBuf::from("/data"), mode, ops)
    }

    /// Serializes every test that swaps the process-global PATH.
    /// PATH is shared by all test threads in this process, so two
    /// PathGuard tests running in parallel can restore each other's
    /// PATH mid-test (the intermittent start-gateway failures: one
    /// test resolves uv against another test's PATH). PathGuard
    /// holds this lock from construction until drop, so only one
    /// swapped PATH is ever live at a time.
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Guard that swaps PATH for the duration of a test. uv resolution
    /// reads the real process PATH, so tests that assert on it must
    /// control it. Restores on drop. Holds [`PATH_LOCK`] for its whole
    /// lifetime so PATH-swapping tests never overlap.
    struct PathGuard {
        old: Option<std::ffi::OsString>,
        // Held purely for its Drop, which releases PATH_LOCK.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl PathGuard {
        fn set_to(dir: &std::path::Path) -> Self {
            // Recover from poisoning: a panicking holder must not
            // wedge every later PATH test in the process.
            let lock = PATH_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let old = std::env::var_os("PATH");
            std::env::set_var("PATH", dir.as_os_str());
            Self { old, _lock: lock }
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match self.old.take() {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// Temp dir acting as a PATH entry. When `with_uv` is true it contains
    /// a fake `uv` binary so uv resolution hits PATH deterministically.
    fn path_dir_with_uv(with_uv: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-wiz-uv-{}-{}",
            std::process::id(),
            if with_uv { "hit" } else { "miss" }
        ));
        std::fs::create_dir_all(&dir).unwrap();
        if with_uv {
            let name = if cfg!(target_os = "windows") {
                "uv.exe"
            } else {
                "uv"
            };
            std::fs::write(dir.join(name), b"fake-uv").unwrap();
        }
        dir
    }

    fn smoke_post_response() -> crate::HttpResponse {
        FakeSystemOps::http_ok(r#"{"choices":[{"message":{"content":"ok"}}]}"#)
    }

    // --- DetectHardware ---

    #[tokio::test]
    async fn detect_hardware_reports_gpus() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let ctx = ctx_with(ops, RunMode::Install);
        let step = DetectHardware;
        assert!(step.is_check());
        assert!(step.prerequisites().is_empty());
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(out.message.contains("3 GPU(s)"), "{}", out.message);
        assert_eq!(out.log_tail.len(), 3);
        assert!(out.log_tail[1].contains("sm_60"));
    }

    #[tokio::test]
    async fn detect_hardware_fails_without_gpus() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(vec![]);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DetectHardware.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no NVIDIA GPUs"));
    }

    #[test]
    fn detect_hardware_offers_driver_download_url() {
        assert_eq!(
            DetectHardware.help_url(),
            Some(crate::NVIDIA_DRIVER_DOWNLOAD_URL)
        );
        assert!(crate::NVIDIA_DRIVER_DOWNLOAD_URL.starts_with("https://"));
        let hint = DetectHardware.remediation_hint().unwrap();
        assert!(hint.contains(crate::NVIDIA_DRIVER_DOWNLOAD_URL));
    }

    #[tokio::test]
    async fn detect_hardware_rejects_old_driver() {
        let ops = Arc::new(FakeSystemOps::new());
        let mut gpus = test_gpus();
        for g in gpus.iter_mut() {
            g.driver_version = "471.11".to_string();
        }
        ops.with_gpus(gpus);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DetectHardware.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("too old"), "{}", out.message);
    }

    #[tokio::test]
    async fn detect_hardware_probe_error_fails() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpu_error("nvidia-smi not found");
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DetectHardware.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
    }

    // --- InstallLlamaCpp ---

    #[tokio::test]
    async fn install_llamacpp_happy_path_and_transparency() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());

        // Transparency: planned commands equal what will run — BOTH
        // archive downloads, binaries first.
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].argv[0], "download");
        assert_eq!(planned[0].argv[1], pin.download_url());
        assert!(
            planned[0].argv[1].contains("cuda-12.4"),
            "{:?}",
            planned[0].argv
        );
        assert_eq!(planned[1].argv[0], "download");
        assert_eq!(planned[1].argv[1], pin.runtime_download_url());

        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("b9999"));

        // Both archives downloaded + extracted; marker records both.
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
        assert!(ops.file_bytes(&archive).is_some());
        assert!(ops.file_bytes(&runtime_archive).is_some());
        assert_eq!(ops.downloads_made().len(), 2);
        let marker_bytes = ops
            .file_bytes(&PathBuf::from("/data/llamacpp/.installed.json"))
            .expect("marker written");
        let marker: serde_json::Value =
            serde_json::from_slice(&marker_bytes).expect("marker is JSON");
        assert_eq!(marker["tag"], "b9999");
        assert_eq!(marker["asset"], pin.asset);
        assert_eq!(marker["sha256"], pin.sha256);
        assert_eq!(marker["runtime_asset"], pin.runtime.asset);
        assert_eq!(marker["runtime_sha256"], pin.runtime.sha256);
        assert_eq!(ops.extractions().len(), 2);
        assert_eq!(ops.extractions()[0].0, archive);
        assert_eq!(ops.extractions()[1].0, runtime_archive);
    }

    #[tokio::test]
    async fn install_llamacpp_skips_redownload_when_verified() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive.clone(), b"fake-zip-bytes".to_vec());
        let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
        ops.with_file(runtime_archive, b"fake-cudart-bytes".to_vec());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(ops.downloads_made().is_empty(), "no download needed");
        assert!(out
            .log_tail
            .iter()
            .any(|l| l.contains("already present with matching checksum")));
    }

    #[tokio::test]
    async fn install_llamacpp_redownloads_only_the_missing_archive() {
        // Binaries archive already verified on disk; the runtime archive
        // is missing, so exactly one download happens (the runtime one).
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive, b"fake-zip-bytes".to_vec());
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin.clone())
            .run(&ctx)
            .await
            .unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let made = ops.downloads_made();
        assert_eq!(made.len(), 1, "{made:?}");
        assert_eq!(made[0].0, pin.runtime_download_url());
    }

    #[tokio::test]
    async fn install_llamacpp_fails_closed_without_pin() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::unconfigured().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no llama.cpp build pinned"));
        // The message must state the real cause (a packaging error in the
        // build) and must NOT direct the operator to configure
        // updates.components — that path cannot supply a pin.
        let detail = out.log_tail.join("\n");
        assert!(detail.contains("packaging error"), "{detail}");
        assert!(!detail.contains("configure updates.components"), "{detail}");
    }

    #[tokio::test]
    async fn install_llamacpp_rejects_cuda13_on_pascal() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus()); // includes P100s
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
            sha256: sha256_hex(b"x"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: sha256_hex(b"y"),
            },
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("cuda-13"), "{}", out.message);
        assert!(ops.downloads_made().is_empty(), "no download attempted");
    }

    #[tokio::test]
    async fn install_llamacpp_rejects_cuda13_runtime_on_pascal() {
        // The Pascal rule covers BOTH archives: a cuda-12.4 binaries
        // archive paired with a cuda-13 runtime archive is still refused,
        // and the error names the runtime asset.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus()); // includes P100s
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"x"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-13.0-x64.zip".to_string(),
                sha256: sha256_hex(b"y"),
            },
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("cuda-13"), "{}", out.message);
        assert!(
            out.message
                .contains("cudart-llama-bin-win-cuda-13.0-x64.zip"),
            "{}",
            out.message
        );
        assert!(ops.downloads_made().is_empty(), "no download attempted");
    }

    #[tokio::test]
    async fn install_llamacpp_allows_cuda13_without_pascal() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(vec![test_gpus()[0].clone()]); // A4000 only
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
            sha256: sha256_hex(b"fake-zip-bytes"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: sha256_hex(b"fake-cudart-bytes"),
            },
        };
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
    }

    #[tokio::test]
    async fn install_llamacpp_checksum_mismatch_fails() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        ops.with_download(&pin.download_url(), b"tampered-bytes".to_vec());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"));
    }

    #[tokio::test]
    async fn install_llamacpp_audit_reverifies_and_detects_tamper() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        script_pin_downloads(&ops, &pin);
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());
        assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

        // Audit passes on the pristine install.
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);

        // Tamper with the archive: audit must fail with a remediation hint.
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive, b"tampered".to_vec());
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"));
        assert!(step.remediation_hint().unwrap().contains("re-download"));
    }

    #[tokio::test]
    async fn install_llamacpp_audit_fails_on_runtime_checksum_mismatch() {
        // Audit verifies BOTH archives: a tampered runtime archive fails
        // even when the binaries archive is pristine, and the message
        // names the runtime archive.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        script_pin_downloads(&ops, &pin);
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());
        assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

        let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
        ops.with_file(runtime_archive, b"tampered-cudart".to_vec());
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"), "{}", out.message);
        assert!(out.message.contains(&pin.runtime.asset), "{}", out.message);
    }

    #[test]
    fn default_pin_is_verified_b11658_cuda124_pair() {
        let pin = LlamaCppPin::default_pin();
        assert_eq!(pin.tag, "b11658");
        assert_eq!(pin.asset, "llama-b11658-bin-win-cuda-12.4-x64.zip");
        assert_eq!(
            pin.sha256,
            "4268543cfffee11e62dbe1fb4ee907da508c2a70de7b845ed7b338bf8cabd76d"
        );
        assert_eq!(pin.runtime.asset, "cudart-llama-bin-win-cuda-12.4-x64.zip");
        assert_eq!(
            pin.runtime.sha256,
            "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"
        );
        for sha in [&pin.sha256, &pin.runtime.sha256] {
            assert_eq!(sha.len(), 64, "{sha}");
            assert!(
                sha.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{sha}"
            );
        }
        for name in [&pin.asset, &pin.runtime.asset] {
            assert!(name.contains("cuda-12.4"), "{name}");
            assert!(!name.contains("cuda-13"), "{name}");
        }
        assert_eq!(
            pin.download_url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11658/llama-b11658-bin-win-cuda-12.4-x64.zip"
        );
        assert_eq!(
            pin.runtime_download_url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11658/cudart-llama-bin-win-cuda-12.4-x64.zip"
        );
        // Regression for the shipped defect: the default step list must
        // carry the pin (it previously shipped unconfigured and step 2
        // could never pass).
        let ctx = WizardContext::new(
            AppConfig::default_config(),
            PathBuf::from("/data"),
            RunMode::Install,
            Arc::new(FakeSystemOps::new()),
        );
        let step = default_steps()
            .into_iter()
            .find(|s| s.id() == STEP_INSTALL_LLAMACPP)
            .expect("llamacpp step present in default steps");
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 2, "{planned:?}");
        assert!(
            planned[0].argv[1].contains("b11658"),
            "{:?}",
            planned[0].argv
        );
        assert!(
            planned[1].argv[1].contains("cudart"),
            "{:?}",
            planned[1].argv
        );
    }

    /// Context whose config pins `updates.components.llamacpp` to the
    /// given version (or leaves the component unpinned when `None`).
    fn ctx_with_llamacpp_pin(
        ops: Arc<FakeSystemOps>,
        mode: RunMode,
        pinned_version: Option<&str>,
    ) -> WizardContext {
        let mut cfg = test_config_with_models();
        cfg.updates.components.insert(
            "llamacpp".to_string(),
            ComponentUpdate {
                pinned_version: pinned_version.map(str::to_string),
                auto_update: false,
                last_checked: None,
            },
        );
        WizardContext::new(cfg, PathBuf::from("/data"), mode, ops)
    }

    #[tokio::test]
    async fn install_llamacpp_config_version_mismatch_fails_closed() {
        // The config names a version this build carries no verified pin
        // for: refused before any download, naming both versions.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin(); // built-in tag b9999
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with_llamacpp_pin(ops.clone(), RunMode::Install, Some("b1111"));
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("b1111"), "{}", out.message);
        assert!(out.message.contains("b9999"), "{}", out.message);
        assert!(
            ops.downloads_made().is_empty(),
            "refused before any download"
        );
    }

    #[tokio::test]
    async fn install_llamacpp_config_version_match_proceeds() {
        // A config pin naming exactly the built-in tag is a no-op guard:
        // the step proceeds with the verified pin.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin(); // built-in tag b9999
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with_llamacpp_pin(ops.clone(), RunMode::Install, Some("b9999"));
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert_eq!(ops.downloads_made().len(), 2);
    }

    // --- ConfigureTensorSplit ---

    #[tokio::test]
    async fn compute_splits_sums_to_one() {
        let splits = compute_splits(&[16384, 16384, 16384]).unwrap();
        assert_eq!(splits.len(), 3);
        let sum: f32 = splits.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "{splits:?}");
        // Uneven VRAM: proportional shares.
        let splits = compute_splits(&[24576, 8192]).unwrap();
        assert!((splits[0] - 0.75).abs() < 1e-6, "{splits:?}");
        assert!((splits[1] - 0.25).abs() < 1e-6);
        // Zero VRAM is an error, not a silent [0,0].
        assert!(compute_splits(&[0, 0]).is_err());
    }

    #[test]
    fn compute_splits_never_negative_and_always_sums_to_one() {
        // Deterministic pseudo-random sweep: adversarial VRAM mixes over
        // 1..=40 GPUs must never produce a negative share or a bad sum.
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        for _ in 0..2000 {
            let n = 1 + (next() % 40) as usize;
            let vrams: Vec<u64> = (0..n).map(|_| 1024 + next() % 49152).collect();
            let splits = compute_splits(&vrams).unwrap();
            assert_eq!(splits.len(), n);
            for s in &splits {
                assert!(
                    (0.0..=1.0).contains(s),
                    "split out of range: {splits:?} from {vrams:?}"
                );
            }
            let sum: f32 = splits.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-4,
                "sum != 1.0: {splits:?} from {vrams:?}"
            );
        }
        // Single GPU owns the whole split.
        assert_eq!(compute_splits(&[16384]).unwrap(), vec![1.0]);
        // Empty input is zero total VRAM, not a panic.
        assert!(compute_splits(&[]).is_err());
    }

    #[tokio::test]
    async fn tensor_split_writes_and_audit_validates() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = ConfigureTensorSplit;
        assert!(step.planned_commands(&install_ctx).is_empty());
        let out = step.run(&install_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("[0.33/0.33/0.34]"), "{}", out.message);

        let path = PathBuf::from("/data/wizard/tensor-split.json");
        let raw = ops.file_bytes(&path).expect("splits file written");
        let doc: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(doc.as_object().unwrap().len(), 4, "one entry per backend");

        // Audit passes.
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");

        // Corrupt the file: audit fails.
        ops.with_file(path, b"{\"planner\": [0.5]}".to_vec());
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("not 1.0") || out.message.contains("GPUs"));
    }

    // --- DownloadModels ---

    #[tokio::test]
    async fn download_models_install_and_verify() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = DownloadModels;
        assert_eq!(step.planned_commands(&ctx).len(), 2);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("2/2"));
        assert_eq!(ops.downloads_made().len(), 2);
    }

    #[tokio::test]
    async fn download_models_skips_disabled_models() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let mut config = test_config_with_models();
        config.models[1].enabled = false;
        let ctx = WizardContext::new(
            config,
            PathBuf::from("/data"),
            RunMode::Install,
            ops.clone(),
        );
        let step = DownloadModels;
        // Planned commands cover enabled models only.
        assert_eq!(step.planned_commands(&ctx).len(), 1);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("1/1"), "{}", out.message);
        assert_eq!(ops.downloads_made().len(), 1);
    }

    #[tokio::test]
    async fn download_models_none_enabled_is_a_no_op() {
        let ops = Arc::new(FakeSystemOps::new());
        let mut config = test_config_with_models();
        for m in &mut config.models {
            m.enabled = false;
        }
        let ctx = WizardContext::new(
            config,
            PathBuf::from("/data"),
            RunMode::Install,
            ops.clone(),
        );
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("no models enabled"), "{}", out.message);
        assert_eq!(ops.downloads_made().len(), 0);
    }

    #[tokio::test]
    async fn download_models_missing_url_fails_closed() {
        let ops = Arc::new(FakeSystemOps::new());
        let mut config = test_config_with_models();
        config.models[0].source_url.clear();
        let ctx = WizardContext::new(config, PathBuf::from("/data"), RunMode::Install, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no source URL"));
    }

    #[tokio::test]
    async fn download_models_audit_detects_corruption() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = DownloadModels;
        assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        assert_eq!(step.run(&audit_ctx).await.unwrap().state, StepState::Done);

        // Corrupt one file on disk.
        ops.with_file(
            PathBuf::from("/data/models/qwen3-8b.gguf"),
            b"corrupted".to_vec(),
        );
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"));
        assert!(step.remediation_hint().unwrap().contains("re-download"));
        // Audit performed no downloads.
        assert_eq!(ops.downloads_made().len(), 2, "audit added no downloads");
    }

    // --- DownloadModels: verified_at stamping ---

    /// Config fixture that passes `manager_config::save` validation.
    /// The shared fixture enables every backend, but its backends point
    /// at model files absent from the replaced models list, which
    /// save() rejects. The stamp tests persist the config, so they
    /// disable the backends (DownloadModels ignores backends).
    fn stamp_test_config() -> AppConfig {
        let mut c = test_config_with_models();
        for b in c.backends.iter_mut() {
            b.enabled = false;
        }
        c
    }

    /// Unique real temp data dir for the stamp tests: the stamp
    /// targets `<data_dir>/config.json` on the real filesystem.
    fn stamp_test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("llm-wiz-stamp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn download_models_install_stamps_verified_at() {
        let data_dir = stamp_test_dir("install");
        let config_path = data_dir.join("config.json");
        // Nothing persisted yet (first run): the stamp falls back to
        // the context config and creates the persisted file.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = WizardContext::new(stamp_test_config(), data_dir.clone(), RunMode::Install, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");

        let persisted = manager_config::load(&config_path).unwrap();
        assert_eq!(persisted.models.len(), 2);
        for m in &persisted.models {
            assert!(m.verified_at.is_some(), "{} verified but not stamped", m.id);
        }
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn download_models_audit_stamps_verified_at() {
        let data_dir = stamp_test_dir("audit");
        let config_path = data_dir.join("config.json");
        // A persisted config with no stamps yet: audit loads it as
        // the stamp base.
        let config = stamp_test_config();
        manager_config::save(&config, &config_path).unwrap();

        // Both model files already installed with matching bytes.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_file(
            data_dir.join("models/qwen3-8b.gguf"),
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_file(
            data_dir.join("models/qwen2.5-coder-7b.gguf"),
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = WizardContext::new(config, data_dir.clone(), RunMode::Audit, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("2/2"), "{out:?}");

        let persisted = manager_config::load(&config_path).unwrap();
        for m in &persisted.models {
            assert!(m.verified_at.is_some(), "{} audited but not stamped", m.id);
        }
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn download_models_mismatch_is_not_stamped() {
        let data_dir = stamp_test_dir("mismatch");
        let config_path = data_dir.join("config.json");
        let config = stamp_test_config();
        manager_config::save(&config, &config_path).unwrap();

        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        // The second model's bytes do not match its pinned hash.
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"tampered-bytes".to_vec(),
        );
        let ctx = WizardContext::new(config, data_dir.clone(), RunMode::Install, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"), "{out:?}");

        let persisted = manager_config::load(&config_path).unwrap();
        let good = persisted
            .models
            .iter()
            .find(|m| m.id == "qwen3-8b")
            .unwrap();
        assert!(
            good.verified_at.is_some(),
            "model verified earlier in the same run is stamped"
        );
        let bad = persisted
            .models
            .iter()
            .find(|m| m.id == "qwen2.5-coder-7b")
            .unwrap();
        assert!(
            bad.verified_at.is_none(),
            "mismatching model must not be stamped"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // --- StartGateway ---

    #[tokio::test]
    async fn start_gateway_install_spawns_and_healthchecks() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 5);
        // Probes first, then the pinned venv creation, pip, and spawn.
        assert!(planned[2].argv[0].ends_with("uv"));
        assert!(planned[2].argv.contains(&"--python".to_string()));
        assert!(planned[2]
            .argv
            .contains(&crate::GATEWAY_PYTHON_VERSION.to_string()));
        assert!(planned[2].argv.contains(&"--clear".to_string()));

        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // No venv exists yet, so the probes execute nothing: venv + pip
        // install ran; gateway spawned with the right port.
        let executed = ops.executed_commands();
        assert_eq!(executed[0], planned[2].argv);
        assert_eq!(executed[1], planned[3].argv);
        let spawns = ops.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0], planned[4].argv);
        assert!(spawns[0].contains(&"4000".to_string()));
        // config.yaml generated from app state.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(text.contains("8081"), "{text}");
    }

    #[tokio::test]
    async fn start_gateway_recreates_venv_on_wrong_python_version() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        // An existing venv whose interpreter reports Python 3.15 — the
        // unpinned failure mode (hiredis has no cp315 wheel). The step
        // must recreate it pinned instead of failing at `uv venv`.
        ops.with_file(step.venv_python(&ctx), Vec::new());
        let probe = step.version_probe_command(&ctx);
        let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &probe_argv,
            crate::CommandOutput {
                exit_code: 0,
                stdout: "3.15\n".to_string(),
                stderr: String::new(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("recreating it with Python")),
            "{out:?}"
        );
        let executed = ops.executed_commands();
        let venv_cmd = executed
            .iter()
            .find(|a| a.iter().any(|x| x == "venv"))
            .expect("venv recreated");
        assert!(venv_cmd.contains(&"--clear".to_string()));
        assert!(venv_cmd.contains(&crate::GATEWAY_PYTHON_VERSION.to_string()));
        assert!(
            executed.iter().any(|a| a.iter().any(|x| x == "install")),
            "pip install ran: {executed:?}"
        );
        assert_eq!(ops.spawns().len(), 1);
    }

    #[tokio::test]
    async fn start_gateway_reuses_current_venv_with_litellm() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        // Venv already on the pinned Python; the version probe reports
        // it and the import probe succeeds by default — no venv or pip
        // command may run, but the gateway still spawns.
        ops.with_file(step.venv_python(&ctx), Vec::new());
        let probe = step.version_probe_command(&ctx);
        let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &probe_argv,
            crate::CommandOutput {
                exit_code: 0,
                stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
                stderr: String::new(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert!(
            executed
                .iter()
                .all(|a| !a.iter().any(|x| x == "venv" || x == "install")),
            "no venv/pip commands may run: {executed:?}"
        );
        assert_eq!(ops.spawns().len(), 1);
    }

    #[tokio::test]
    async fn start_gateway_spawn_uses_litellm_cli_and_captures_logs() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let cmds = ops.spawn_cmds();
        assert_eq!(cmds.len(), 1);
        // The console script, never `python -m litellm` (no __main__).
        assert!(
            cmds[0].argv[0].ends_with("litellm") || cmds[0].argv[0].ends_with("litellm.exe"),
            "{:?}",
            cmds[0].argv
        );
        assert!(
            !cmds[0].argv.iter().any(|a| a == "-m"),
            "{:?}",
            cmds[0].argv
        );
        // Output capture is wired to the gateway log files.
        assert_eq!(
            cmds[0].stdout_log.as_deref(),
            Some(std::path::Path::new("/data/logs/gateway.stdout.log"))
        );
        assert_eq!(
            cmds[0].stderr_log.as_deref(),
            Some(std::path::Path::new("/data/logs/gateway.stderr.log"))
        );
        // A master key was generated, stored, and injected into the
        // gateway's environment (current LiteLLM refuses to boot
        // without one).
        let key = cmds[0]
            .env
            .get("LITELLM_MASTER_KEY")
            .expect("master key env set");
        // UTF-8 stdio is forced: with redirected output, cp1252
        // cannot encode LiteLLM's startup banner and startup fails.
        assert_eq!(
            cmds[0].env.get("PYTHONIOENCODING").map(String::as_str),
            Some("utf-8")
        );
        assert_eq!(cmds[0].env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert!(key.starts_with("sk-") && key.len() > 32, "{key}");
        assert_eq!(
            ops.credential_value(crate::LITELLM_MASTER_KEY_CRED_TARGET)
                .as_deref(),
            Some(key.as_str()),
            "stored credential matches the injected key"
        );
    }

    #[tokio::test]
    async fn start_gateway_fails_fast_when_process_exits() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        // The spawned gateway exits immediately; its captured stderr
        // carries the interpreter error the operator needs to see.
        ops.with_dead_spawn();
        ops.with_file(
            PathBuf::from("/data/logs/gateway.stderr.log"),
            b"C:\\venv\\python.exe: No module named litellm.__main__; 'litellm' is a package and cannot be directly executed\n".to_vec(),
        );
        let step = StartGateway {
            health_retries: 30,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        // Detected on the first probe, not after the full budget.
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("exited during startup (probe attempt 1)")),
            "{out:?}"
        );
        // The captured gateway stderr tail rides in the outcome.
        assert!(
            out.log_tail.iter().any(|l| l.contains("No module named")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn start_gateway_install_stops_previous_and_records_pid() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        // A previous gateway (pid 7777) is recorded and still running.
        ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
        ops.with_live_pid(7777);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // The old process was terminated before the replacement spawned,
        // and the pid file now records the new spawn (fake pid 4242).
        assert_eq!(ops.killed_pids(), vec![7777]);
        assert_eq!(ops.spawns().len(), 1);
        let pid_file = ops
            .file_bytes(&PathBuf::from("/data/gateway.pid"))
            .expect("pid file written");
        assert_eq!(String::from_utf8(pid_file).unwrap(), "4242");
    }

    #[tokio::test]
    async fn start_gateway_install_fails_when_previous_cannot_be_stopped() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
        ops.with_live_pid(7777);
        ops.with_kill_failure();
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(
            out.message.contains("could not stop the previous gateway"),
            "{out:?}"
        );
        // No duplicate was spawned over the still-running old process.
        assert!(ops.spawns().is_empty());
    }

    #[tokio::test]
    async fn start_gateway_audit_does_not_stop_recorded_gateway() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok("{}"),
        );
        ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
        ops.with_live_pid(7777);
        let ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = StartGateway::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(ops.killed_pids().is_empty(), "audit kills nothing");
        assert!(ops.spawns().is_empty(), "audit spawns nothing");
    }

    fn gateway_ctx_with_openrouter(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
        let mut cfg = test_config_with_models();
        cfg.gateway.openrouter.enabled = true;
        cfg.gateway.openrouter.cred_ref = Some("LocalLLM/OpenRouter".to_string());
        cfg.gateway.openrouter.model_allowlist = vec!["openai/gpt-4o-mini".to_string()];
        WizardContext::new(cfg, PathBuf::from("/data"), mode, ops)
    }

    /// Script the bootstrapped uv in the fake fs so gateway tests do
    /// not need the PATH-mutating PathGuard (parallel tests share the
    /// process PATH; these tests must not race the ones that use it).
    fn with_bundled_uv(ops: &FakeSystemOps) {
        let name = if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        };
        ops.with_file(PathBuf::from("/data/bin").join(name), b"fake-uv".to_vec());
    }

    #[tokio::test]
    async fn start_gateway_injects_openrouter_key_from_credential() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("LocalLLM/OpenRouter", "sk-test-value");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = gateway_ctx_with_openrouter(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        // The plan records the variable but never the value.
        let planned = step.planned_commands(&ctx);
        let plan_spawn = planned.last().unwrap();
        assert_eq!(
            plan_spawn.env.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("<set from credential manager>")
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // The spawned gateway carries the key in its environment.
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-test-value")
        );
        // Loopback bind on the spawn argv.
        let spawns = ops.spawns();
        let host = spawns[0]
            .windows(2)
            .find(|w| w[0] == "--host")
            .expect("--host present");
        assert_eq!(host[1], "127.0.0.1");
        // The key never lands in the generated YAML.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(!text.contains("sk-test-value"), "{text}");
    }

    #[tokio::test]
    async fn start_gateway_warns_when_openrouter_credential_unreadable() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        // No credential scripted: credential_read returns None.
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = gateway_ctx_with_openrouter(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        // The step still succeeds; the outcome carries the warning and
        // the spawn has no key.
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("OpenRouter fallback is not armed"),
            "{out:?}"
        );
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert!(!envs[0].contains_key("OPENROUTER_API_KEY"));
    }

    #[tokio::test]
    async fn start_gateway_fails_when_unhealthy() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        let ops = Arc::new(FakeSystemOps::new());
        // No scripted health response → probes fail.
        let ctx = ctx_with(ops, RunMode::Install);
        let step = StartGateway {
            health_retries: 2,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("did not become healthy"));
    }

    #[tokio::test]
    async fn start_gateway_audit_probes_health() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok("{}"),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = StartGateway::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(ops.spawns().is_empty(), "audit spawns nothing");
    }

    #[test]
    fn find_uv_in_detects_binary() {
        let dir = path_dir_with_uv(true);
        let found = StartGateway::find_uv_in(std::slice::from_ref(&dir));
        assert!(found.is_some());
        let empty = path_dir_with_uv(false);
        assert!(StartGateway::find_uv_in(std::slice::from_ref(&empty)).is_none());
        assert!(StartGateway::find_uv_in(&[]).is_none());
    }

    #[tokio::test]
    async fn start_gateway_uses_bundled_uv_when_present() {
        // uv not on PATH, but the bootstrapped copy exists in the fake fs.
        let _guard = PathGuard::set_to(&path_dir_with_uv(false));
        let ops = Arc::new(FakeSystemOps::new());
        let name = if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        };
        ops.with_file(PathBuf::from("/data/bin").join(name), b"fake-uv".to_vec());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert!(executed[0][0].ends_with(name), "{:?}", executed[0]);
        // No bootstrap download happened.
        assert!(ops.downloaded_to().is_empty());
    }

    #[tokio::test]
    async fn start_gateway_planned_shows_bootstrap_when_uv_missing() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(false));
        let ctx = ctx_with(Arc::new(FakeSystemOps::new()), RunMode::Install);
        let planned = StartGateway::default().planned_commands(&ctx);
        if cfg!(target_os = "windows") {
            assert_eq!(planned.len(), 6);
            assert_eq!(planned[0].argv[0], "download");
            assert!(
                planned[0].argv[1].contains("astral-sh/uv"),
                "{:?}",
                planned[0].argv
            );
        } else {
            // No auto-bootstrap off Windows: uv path shown, run() fails
            // with install guidance instead.
            assert_eq!(planned.len(), 5);
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn start_gateway_fails_with_install_guidance_without_uv() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(false));
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = StartGateway::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(
            out.message
                .contains("auto-bootstrap is only supported on Windows"),
            "{out:?}"
        );
    }

    // --- InstallLlamaCpp: VC++ redistributable ---

    #[test]
    fn vcredist_check_passes_off_windows() {
        // The probe is Windows-only; other platforms always pass.
        assert!(InstallLlamaCpp::vcredist_present(&PathBuf::from(
            "/nonexistent-system32"
        )));
    }

    #[test]
    fn llamacpp_offers_vcredist_download_url() {
        assert_eq!(
            InstallLlamaCpp::with_pin(test_pin()).help_url(),
            Some(crate::VC_REDIST_DOWNLOAD_URL)
        );
        assert!(crate::VC_REDIST_DOWNLOAD_URL.starts_with("https://"));
    }

    // --- DownloadModels: disk-space pre-check ---

    #[tokio::test]
    async fn download_models_fails_when_disk_full() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_head_length("https://example.com/qwen3-8b.gguf", 50_000_000_000);
        ops.with_head_length("https://example.com/qwen2.5-coder-7b.gguf", 40_000_000_000);
        ops.with_free_space(1_000_000);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("insufficient disk space"), "{out:?}");
    }

    #[tokio::test]
    async fn download_models_proceeds_when_space_sufficient() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_head_length("https://example.com/qwen3-8b.gguf", 5_000_000_000);
        ops.with_head_length("https://example.com/qwen2.5-coder-7b.gguf", 4_000_000_000);
        ops.with_free_space(100_000_000_000);
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
    }

    // --- VscodeExtensions ---

    #[tokio::test]
    async fn vscode_install_writes_config_without_clobbering() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = VscodeExtensions::default();
        assert_eq!(step.planned_commands(&ctx).len(), 2);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // The `code --version` probe plus two extension installs ran.
        assert_eq!(ops.executed_commands().len(), 3);
        // Canonical config written (nothing existed).
        let canonical = ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.json"))
            .expect("canonical config written");
        let text = String::from_utf8(canonical).unwrap();
        assert!(text.contains("http://127.0.0.1:4000/v1"), "{text}");
        assert!(text.contains("planner"), "{text}");

        // Second run: existing config is left alone, sidecar written instead.
        let out2 = step.run(&ctx).await.unwrap();
        assert_eq!(out2.state, StepState::Done);
        assert!(ops
            .file_bytes(&PathBuf::from(
                "/home/test/.continue/config.json.llm-manager"
            ))
            .is_some());
    }

    #[tokio::test]
    async fn vscode_install_skips_when_code_cli_missing() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(
            &["code", "--version"],
            -1,
            "spawn failed: program not found",
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped, "{out:?}");
        assert!(out.message.contains("not installed"), "{}", out.message);
        // Only the probe ran — no install was attempted.
        assert_eq!(ops.executed_commands().len(), 1);
    }

    #[tokio::test]
    async fn vscode_audit_skips_when_code_cli_missing() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(
            &["code", "--list-extensions"],
            -1,
            "spawn failed: program not found",
        );
        let ctx = ctx_with(ops, RunMode::Audit);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped, "{out:?}");
    }

    #[tokio::test]
    async fn vscode_audit_checks_presence() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_listed_extensions(&["continue.continue", "saoudrizwan.claude-dev", "other"]);
        let ctx = ctx_with(ops, RunMode::Audit);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);

        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_listed_extensions(&["continue.continue"]);
        let ctx2 = ctx_with(ops2, RunMode::Audit);
        let out2 = VscodeExtensions::default().run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Failed);
        assert!(out2.message.contains("saoudrizwan.claude-dev"));
    }

    // --- McpServers ---

    #[tokio::test]
    async fn mcp_servers_requires_credentials() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = McpServers.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        // Target name shown; values never shown (there are none to show).
        assert!(out.message.contains(GITHUB_CRED_TARGET), "{}", out.message);

        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_credential(GITHUB_CRED_TARGET);
        ops2.with_credential(GDRIVE_CRED_TARGET);
        let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
        let out2 = McpServers.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        let raw = ops2
            .file_bytes(&PathBuf::from("/data/mcp/mcp-servers.json"))
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("credential-manager:"), "{text}");
        assert!(text.contains(GITHUB_CRED_TARGET), "{text}");
    }

    // --- WinMlRegister ---

    fn winml_ctx(ops: Arc<FakeSystemOps>, mode: RunMode, model_id: Option<&str>) -> WizardContext {
        let mut config = test_config_with_models();
        config.winml.enabled = true;
        config.winml.model_id = model_id.map(str::to_string);
        WizardContext::new(config, PathBuf::from("/data"), mode, ops)
    }

    #[tokio::test]
    async fn winml_disabled_skips() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install); // winml disabled by default
        let out = WinMlRegister.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped);
    }

    #[tokio::test]
    async fn winml_registers_small_model_with_limits() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:8090/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = winml_ctx(ops.clone(), RunMode::Install, Some("qwen3-8b"));
        let step = WinMlRegister;
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let raw = ops
            .file_bytes(&PathBuf::from("/data/winml/registration.json"))
            .expect("registration written");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["model_id"], "qwen3-8b");
        assert_eq!(v["limits"]["supports_tensor_split"], false);
        assert_eq!(v["limits"]["experimental"], true);
    }

    #[tokio::test]
    async fn winml_rejects_large_model_class() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = winml_ctx(ops, RunMode::Install, Some("qwen3-32b"));
        let out = WinMlRegister.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("ineligible"), "{}", out.message);
    }

    #[tokio::test]
    async fn winml_fails_when_server_down() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = winml_ctx(ops, RunMode::Install, Some("qwen3-8b"));
        let out = WinMlRegister.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("not reachable"));
        assert!(WinMlRegister
            .remediation_hint()
            .unwrap()
            .contains("WinMLServer"));
    }

    // --- MxcPolicies ---

    #[tokio::test]
    async fn mxc_policy_written_validated_and_self_tested() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command(
            &["wxc-exec.exe", "--probe-json"],
            FakeSystemOps::ok_output(r#"{"psec":"1.1","ingress_loopback":true}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = MxcPolicies;
        assert_eq!(step.planned_commands(&ctx).len(), 2);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("Learning"), "{}", out.message);
        let raw = ops
            .file_bytes(&PathBuf::from("/data/mxc/policy.json"))
            .expect("policy written");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["default_deny"], true);
        // Self-test ran against the written policy path.
        let executed = ops.executed_commands();
        assert!(executed
            .iter()
            .any(|a| a.contains(&"--self-test".to_string())));
    }

    #[tokio::test]
    async fn mxc_audit_revalidates_policy() {
        let ops = Arc::new(FakeSystemOps::new());
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        assert_eq!(
            MxcPolicies.run(&install_ctx).await.unwrap().state,
            StepState::Done
        );
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = MxcPolicies.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
    }

    #[tokio::test]
    async fn mxc_probe_failure_fails_step() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(&["wxc-exec.exe", "--probe-json"], 1, "not found");
        let ctx = ctx_with(ops, RunMode::Install);
        let out = MxcPolicies.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("not available"), "{}", out.message);
    }

    // --- SmokeTest ---

    #[tokio::test]
    async fn smoke_test_passes_and_writes_baselines() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            smoke_post_response(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = SmokeTest::default();
        assert_eq!(step.planned_commands(&ctx).len(), 4, "one probe per group");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("4 group(s)"));
        assert_eq!(ops.posts().len(), 4);
        // Tag present in every request body.
        for (_, body) in ops.posts() {
            assert!(body.contains("wizard smoke test wizard-"), "{body}");
        }
        let raw = ops
            .file_bytes(&PathBuf::from("/data/wizard/smoke-baselines.json"))
            .expect("baselines written");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["baselines"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn smoke_test_fails_on_empty_completion() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            FakeSystemOps::http_ok(r#"{"choices":[{"message":{"content":"  "}}]}"#),
        );
        let ctx = ctx_with(ops, RunMode::Install);
        let out = SmokeTest::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("empty completion"));
    }

    #[tokio::test]
    async fn smoke_test_fails_on_http_error_status() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            crate::HttpResponse {
                status: 500,
                body: b"boom".to_vec(),
                elapsed_ms: 5,
            },
        );
        let ctx = ctx_with(ops, RunMode::Install);
        let out = SmokeTest::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("500"));
    }

    // --- Step registry invariants ---

    #[test]
    fn default_steps_have_unique_ids_and_valid_prereq_order() {
        let steps = default_steps();
        assert_eq!(steps.len(), 10);
        let ids: Vec<&str> = steps.iter().map(|s| s.id()).collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(*id), "duplicate id {id}");
        }
        // Every prerequisite names an earlier step (topological order).
        for (i, s) in steps.iter().enumerate() {
            for pre in s.prerequisites() {
                let pos = ids.iter().position(|id| *id == pre.as_str());
                assert!(
                    pos.map(|p| p < i).unwrap_or(false),
                    "step {} has bad prerequisite {pre}",
                    s.id()
                );
            }
        }
        // All ten are checks (audit covers everything).
        assert!(steps.iter().all(|s| s.is_check()));
    }

    #[test]
    fn every_step_has_a_remediation_hint() {
        for s in default_steps() {
            assert!(
                s.remediation_hint().is_some(),
                "step {} lacks a remediation hint",
                s.id()
            );
        }
    }

    #[test]
    fn planned_commands_match_mode() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        // Install mode: StartGateway shows the five commands it may
        // spawn (two venv probes, venv creation, pip install, spawn)...
        let install_ctx = WizardContext::new(
            test_config_with_models(),
            PathBuf::from("/data"),
            RunMode::Install,
            Arc::new(FakeSystemOps::new()),
        );
        let install_cmds = StartGateway::default().planned_commands(&install_ctx);
        assert_eq!(install_cmds.len(), 5);
        assert!(install_cmds[2].argv[0].ends_with("uv"));

        // ...audit mode shows only the health probe.
        let audit_ctx = WizardContext::new(
            test_config_with_models(),
            PathBuf::from("/data"),
            RunMode::Audit,
            Arc::new(FakeSystemOps::new()),
        );
        let audit_cmds = StartGateway::default().planned_commands(&audit_ctx);
        assert_eq!(audit_cmds.len(), 1);
        assert_eq!(audit_cmds[0].argv[0], "http-get");

        // VscodeExtensions: install commands vs list-extensions probe.
        let install_cmds = VscodeExtensions::default().planned_commands(&install_ctx);
        assert!(install_cmds.iter().all(|c| c.argv[0] == "code"));
        assert!(install_cmds
            .iter()
            .all(|c| c.argv.contains(&"--install-extension".to_string())));
        let audit_cmds = VscodeExtensions::default().planned_commands(&audit_ctx);
        assert_eq!(audit_cmds.len(), 1);
        assert_eq!(
            audit_cmds[0].argv,
            vec!["code".to_string(), "--list-extensions".to_string()]
        );

        // DetectHardware: the per-GPU query plus the driver-version header
        // probe that RealSystemOps::gpu_inventory actually runs. argv[0]
        // is telemetry's resolved smi path (bare "nvidia-smi" unless a
        // System32 copy exists).
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        let hw_cmds = DetectHardware.planned_commands(&install_ctx);
        assert_eq!(hw_cmds.len(), 2);
        assert_eq!(hw_cmds[0].argv[0], smi);
        assert!(hw_cmds[0].argv.iter().any(|a| a.contains("--query-gpu")));
        assert_eq!(hw_cmds[1].argv, vec![smi]);
    }
}
