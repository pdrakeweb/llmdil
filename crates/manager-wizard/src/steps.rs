//! The ten concrete wizard steps (spec §4.1).
//!
//! Steps branch on [`RunMode`]: in `Install` they perform the install; in
//! `Audit` they re-verify without reinstalling. All system effects go through
//! [`SystemOps`](crate::SystemOps), so every step is hermetic under
//! [`FakeSystemOps`](crate::fake::FakeSystemOps).

use crate::{
    checksums_match, PlannedCommand, RunMode, StepOutcome, WizardContext, WizardError, WizardStep,
};
use async_trait::async_trait;
use manager_config::AppConfig;
use std::path::{Path, PathBuf};

/// Generate the Continue `config.json` content with every gateway model
/// group as a model pointing at the local LiteLLM gateway.
///
/// Shared by the [`VscodeExtensions`] wizard step and the Tauri
/// `vscode_write_config` command so both write byte-identical configs.
pub fn continue_editor_config(config: &AppConfig) -> String {
    let models: Vec<serde_json::Value> = config
        .gateway
        .groups
        .iter()
        .map(|g| {
            serde_json::json!({
                "title": format!("{} (local)", g.name),
                "provider": "openai",
                "model": g.name,
                "apiBase": format!("http://127.0.0.1:{}/v1", config.gateway.port),
            })
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
        Box::new(InstallLlamaCpp::unconfigured()),
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
        // version.
        vec![
            PlannedCommand::new(vec![
                "nvidia-smi".to_string(),
                "--query-gpu=index,name,uuid,memory.total,compute_cap".to_string(),
                "--format=csv,noheader".to_string(),
            ]),
            PlannedCommand::new(vec!["nvidia-smi".to_string()]),
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

/// Pinned llama.cpp build (spec §3.4).
///
/// The pin is data, not code: the tag/SHA-256 come from the update manifest
/// (`updates.components.llamacpp`). [`LlamaCppPin::validate_for_gpus`]
/// enforces the Pascal rule — cuda-12.4 assets carry the sm_60 targets,
/// cuda-13.x dropped them.
#[derive(Debug, Clone)]
pub struct LlamaCppPin {
    pub tag: String,
    pub asset: String,
    pub sha256: String,
}

impl LlamaCppPin {
    pub const BASE_URL: &'static str = "https://github.com/ggml-org/llama.cpp/releases/download";

    pub fn download_url(&self) -> String {
        format!("{}/{}/{}", Self::BASE_URL, self.tag, self.asset)
    }

    /// Refuse cuda-13.x assets on Pascal-bearing machines (spec §3.4).
    pub fn validate_for_gpus(&self, gpus: &[crate::GpuDescriptor]) -> Result<(), WizardError> {
        let asset = self.asset.to_lowercase();
        let is_cuda13 = asset.contains("cuda-13") || asset.contains("cuda13");
        let has_pascal = gpus.iter().any(|g| g.is_pascal_or_older());
        if is_cuda13 && has_pascal {
            return Err(WizardError::UnsupportedBuild {
                asset: self.asset.clone(),
                reason: "cuda-13.x builds dropped Pascal (sm_60) support; \
                         pin a cuda-12.4 asset (spec §3.4)"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// Step 2: download the pinned llama.cpp CUDA build, verify SHA-256,
/// extract to the app dir. Audit mode re-verifies the installed build.
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

    fn download_command(&self, ctx: &WizardContext) -> Option<PlannedCommand> {
        let pin = self.pin.as_ref()?;
        let dest = self.install_dir(ctx).join(&pin.asset);
        Some(PlannedCommand::new(vec![
            "download".to_string(),
            pin.download_url(),
            dest.display().to_string(),
        ]))
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
        self.download_command(ctx).into_iter().collect()
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
                        "configure updates.components.llamacpp with a pinned cuda-12.4 asset (spec §3.4)".to_string(),
                        "failing closed: will not download an unpinned binary".to_string(),
                    ],
                ))
            }
        };
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
        let archive = dir.join(&pin.asset);
        let marker = dir.join(".installed.json");
        let mut log = vec![format!("pinned build: {} ({})", pin.asset, pin.tag)];

        let archive_ok = ctx.ops.file_exists(&archive).await
            && matches!(ctx.ops.sha256_file(&archive).await, Ok(h) if checksums_match(&h, &pin.sha256));

        match ctx.mode {
            RunMode::Install => {
                if !archive_ok {
                    let url = pin.download_url();
                    log.push(format!("downloading {url}"));
                    // The planned command must equal the spawned action: the
                    // download below is exactly planned_commands()[0].
                    match ctx.ops.download(&url, &archive, &|_, _| {}).await {
                        Ok(bytes) => log.push(format!("downloaded {bytes} bytes")),
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                    let actual = match ctx.ops.sha256_file(&archive).await {
                        Ok(h) => h,
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    };
                    if !checksums_match(&actual, &pin.sha256) {
                        return Ok(StepOutcome::failed(
                            format!(
                                "checksum mismatch for {}: expected {}, got {}",
                                archive.display(),
                                pin.sha256,
                                actual
                            ),
                            vec!["archive rejected; not installed".to_string()],
                        ));
                    }
                    log.push("SHA-256 verified".to_string());
                } else {
                    log.push("archive already present with matching checksum".to_string());
                }
                if let Err(e) = ctx.ops.extract_zip(&archive, &dir).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(format!("extracted to {}", dir.display()));
                let marker_json = serde_json::json!({
                    "tag": pin.tag,
                    "asset": pin.asset,
                    "sha256": pin.sha256,
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
                // Re-verify without reinstalling: marker + archive checksum.
                if !ctx.ops.file_exists(&marker).await {
                    return Ok(StepOutcome::failed(
                        "llama.cpp install marker missing",
                        vec![format!("{} not found", marker.display())],
                    ));
                }
                match ctx.ops.sha256_file(&archive).await {
                    Ok(h) if checksums_match(&h, &pin.sha256) => Ok(StepOutcome::done(
                        format!("llama.cpp {} build verified ({})", pin.tag, pin.asset),
                        log,
                    )),
                    Ok(h) => Ok(StepOutcome::failed(
                        format!(
                            "installed build checksum mismatch: expected {}, got {}",
                            pin.sha256, h
                        ),
                        log,
                    )),
                    Err(e) => Ok(StepOutcome::failed(
                        format!("cannot read installed archive: {e}"),
                        vec![format!("{e:?}")],
                    )),
                }
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
            .filter(|m| !m.source_url.is_empty())
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
        let mut log = Vec::new();
        let mut verified = 0;
        if ctx.mode == RunMode::Install {
            if let Err(outcome) = self.check_disk_space(ctx, &mut log).await {
                return Ok(outcome);
            }
        }
        for model in &ctx.config.models {
            let dest = resolve_dest(&ctx.data_dir, &model.gguf_path);
            if model.source_url.is_empty() {
                return Ok(StepOutcome::failed(
                    format!("no source URL configured for model '{}'", model.id),
                    vec!["set models[].source_url in the config (spec §4.6)".to_string()],
                ));
            }
            let url = model.source_url.clone();
            let verify = |actual: &str| checksums_match(actual, &model.sha256);
            match ctx.mode {
                RunMode::Install => {
                    let mut needs_download = true;
                    if ctx.ops.file_exists(&dest).await {
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
                    if needs_download {
                        log.push(format!("downloading {} → {}", url, dest.display()));
                        if let Err(e) = ctx.ops.download(&url, &dest, &|_, _| {}).await {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                        }
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
                    }
                    verified += 1;
                }
                RunMode::Audit => {
                    if !ctx.ops.file_exists(&dest).await {
                        return Ok(StepOutcome::failed(
                            format!("model '{}' missing at {}", model.id, dest.display()),
                            vec![],
                        ));
                    }
                    match ctx.ops.sha256_file(&dest).await {
                        Ok(h) if verify(&h) => {
                            log.push(format!("{}: checksum verified", model.id));
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
        Ok(StepOutcome::done(
            format!("{verified}/{} model(s) verified", ctx.config.models.len()),
            log,
        ))
    }
}

impl DownloadModels {
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
            if model.source_url.is_empty() {
                continue; // the per-model loop reports this properly
            }
            let dest = resolve_dest(&ctx.data_dir, &model.gguf_path);
            let needs_download = if ctx.ops.file_exists(&dest).await {
                !matches!(ctx.ops.sha256_file(&dest).await, Ok(h) if checksums_match(&h, &model.sha256))
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
            health_retries: 30,
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
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "venv".to_string(),
            self.venv_dir(ctx).display().to_string(),
        ])
    }

    fn pip_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            self.venv_python(ctx).display().to_string(),
            self.litellm_spec(ctx),
        ])
    }

    fn spawn_command(&self, ctx: &WizardContext) -> PlannedCommand {
        let cfg = ctx.data_dir.join("gateway").join("config.yaml");
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-m".to_string(),
            "litellm".to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
            "--port".to_string(),
            ctx.config.gateway.port.to_string(),
        ])
    }

    async fn wait_healthy(&self, ctx: &WizardContext, log: &mut Vec<String>) -> bool {
        let url = self.gateway_url(ctx);
        for attempt in 1..=self.health_retries {
            match ctx.ops.http_get(&url).await {
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
            // Exactly the commands run() spawns, in order (plus the uv
            // bootstrap download when uv is not yet resolvable).
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
                cmds.push(self.venv_command(ctx, &uv));
                cmds.push(self.pip_command(ctx, &uv));
                cmds.push(self.spawn_command(ctx));
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
                for cmd in [self.venv_command(ctx, &uv), self.pip_command(ctx, &uv)] {
                    log.push(format!("running: {}", cmd.argv.join(" ")));
                    match ctx.ops.run_command(&cmd).await {
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
                let spawn = self.spawn_command(ctx);
                let pid = match ctx.ops.spawn_detached(&spawn).await {
                    Ok(pid) => pid,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                log.push(format!("LiteLLM spawned (pid {pid})"));
                if !self.wait_healthy(ctx, &mut log).await {
                    return Ok(StepOutcome::failed(
                        format!(
                            "gateway did not become healthy on :{} after {} probes",
                            ctx.config.gateway.port, self.health_retries
                        ),
                        log,
                    ));
                }
                Ok(StepOutcome::done(
                    format!("LiteLLM gateway running on :{}", ctx.config.gateway.port),
                    log,
                ))
            }
            RunMode::Audit => {
                // Re-probe backend health without reinstalling.
                match ctx.ops.http_get(&self.gateway_url(ctx)).await {
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
    /// Generate a Continue config with every gateway group as a model.
    fn continue_config_json(&self, ctx: &WizardContext) -> String {
        continue_editor_config(&ctx.config)
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
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                }
                let config_json = self.continue_config_json(ctx);
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
            let resp = match ctx.ops.http_post(&url, &body).await {
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
    use manager_config::{AppConfig, ModelConfig};
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
        }
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
            },
        ];
        c
    }

    fn ctx_with(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
        WizardContext::new(test_config_with_models(), PathBuf::from("/data"), mode, ops)
    }

    /// Guard that swaps PATH for the duration of a test. uv resolution
    /// reads the real process PATH, so tests that assert on it must
    /// control it. Restores on drop.
    struct PathGuard {
        old: Option<std::ffi::OsString>,
    }

    impl PathGuard {
        fn set_to(dir: &std::path::Path) -> Self {
            let old = std::env::var_os("PATH");
            std::env::set_var("PATH", dir.as_os_str());
            Self { old }
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
        ops.with_download(&pin.download_url(), b"fake-zip-bytes".to_vec());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());

        // Transparency: planned commands equal what will run.
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].argv[0], "download");
        assert!(
            planned[0].argv[1].contains("cuda-12.4"),
            "{:?}",
            planned[0].argv
        );

        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("b9999"));

        // Archive + marker written, extraction recorded.
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        assert!(ops.file_bytes(&archive).is_some());
        assert!(ops
            .file_bytes(&PathBuf::from("/data/llamacpp/.installed.json"))
            .is_some());
        assert_eq!(ops.extractions().len(), 1);
        assert_eq!(ops.extractions()[0].0, archive);
    }

    #[tokio::test]
    async fn install_llamacpp_skips_redownload_when_verified() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive.clone(), b"fake-zip-bytes".to_vec());
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
    async fn install_llamacpp_fails_closed_without_pin() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::unconfigured().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no llama.cpp build pinned"));
    }

    #[tokio::test]
    async fn install_llamacpp_rejects_cuda13_on_pascal() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus()); // includes P100s
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
            sha256: sha256_hex(b"x"),
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("cuda-13"), "{}", out.message);
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
        };
        ops.with_download(&pin.download_url(), b"fake-zip-bytes".to_vec());
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
        ops.with_download(&pin.download_url(), b"fake-zip-bytes".to_vec());
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
        assert_eq!(planned.len(), 3);
        assert!(planned[0].argv[0].ends_with("uv"));

        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // venv + pip install ran; gateway spawned with the right port.
        let executed = ops.executed_commands();
        assert_eq!(executed[0], planned[0].argv);
        assert_eq!(executed[1], planned[1].argv);
        let spawns = ops.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0], planned[2].argv);
        assert!(spawns[0].contains(&"4000".to_string()));
        // config.yaml generated from app state.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(text.contains("8081"), "{text}");
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
            assert_eq!(planned.len(), 4);
            assert_eq!(planned[0].argv[0], "download");
            assert!(
                planned[0].argv[1].contains("astral-sh/uv"),
                "{:?}",
                planned[0].argv
            );
        } else {
            // No auto-bootstrap off Windows: uv path shown, run() fails
            // with install guidance instead.
            assert_eq!(planned.len(), 3);
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
        // Two extension installs ran.
        assert_eq!(ops.executed_commands().len(), 2);
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
        // Install mode: StartGateway shows the three spawned commands...
        let install_ctx = WizardContext::new(
            test_config_with_models(),
            PathBuf::from("/data"),
            RunMode::Install,
            Arc::new(FakeSystemOps::new()),
        );
        let install_cmds = StartGateway::default().planned_commands(&install_ctx);
        assert_eq!(install_cmds.len(), 3);
        assert!(install_cmds[0].argv[0].ends_with("uv"));

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
        // probe that RealSystemOps::gpu_inventory actually runs.
        let hw_cmds = DetectHardware.planned_commands(&install_ctx);
        assert_eq!(hw_cmds.len(), 2);
        assert_eq!(hw_cmds[0].argv[0], "nvidia-smi");
        assert!(hw_cmds[0].argv.iter().any(|a| a.contains("--query-gpu")));
        assert_eq!(hw_cmds[1].argv, vec!["nvidia-smi".to_string()]);
    }
}
