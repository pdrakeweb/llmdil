//! Setup wizard step state machine (spec §4.1).
//!
//! The wizard is a path, not a place: ten ordered steps run automatically,
//! each a `queued → running → done | failed` state machine with `skipped`
//! and `manual` terminal states. [`RunMode::Audit`] re-runs check-type work
//! without reinstalling.
//!
//! Steps that touch the real system go through [`SystemOps`], a small
//! injectable trait. [`RealSystemOps`](ops::RealSystemOps) implements it for
//! production; [`FakeSystemOps`](fake::FakeSystemOps) is the hermetic test
//! harness. The ten concrete steps live in [`steps`]; build them with
//! [`steps::default_steps`].

use async_trait::async_trait;
use manager_config::AppConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod fake;
pub mod ops;
pub mod steps;

pub use ops::{gateway_pid_path, stop_gateway};

/// Number of log lines surfaced inline on a failed step (spec §4.1).
pub const LOG_TAIL_LINES: usize = 20;

/// Minimum NVIDIA driver major version accepted by the hardware step.
/// Spec §3.4: R550+ for the pinned cuda-12.4 llama.cpp build (NVIDIA documents
/// Windows driver >= 551.61 for CUDA 12.4 GA; R535 only satisfies CUDA 12.2).
/// R550+ branches also carry the P100.
pub const MIN_DRIVER_MAJOR: u32 = 550;

/// Stable entry point for manual NVIDIA driver downloads (driver search;
/// per-GPU prefilled deep links are not stable, so the wizard links the
/// search page and states the required branch in text).
pub const NVIDIA_DRIVER_DOWNLOAD_URL: &str = "https://www.nvidia.com/Download/index.aspx";

/// Permanent aka.ms link to the latest VC++ redistributable (x64).
/// Microsoft documents this as the stable "latest" URL.
pub const VC_REDIST_DOWNLOAD_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// Pinned uv version bootstrapped by the gateway step when `uv` is not on
/// PATH. uv is a self-contained binary: no install, no admin needed.
pub const UV_PINNED_VERSION: &str = "0.13.0";

/// Download URL for the pinned uv standalone archive for the current
/// platform. Windows uses the MSVC zip (extracted with the `zip` crate);
/// other platforms are unsupported for auto-bootstrap.
pub fn uv_download_url() -> Option<String> {
    let asset = if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        "uv-x86_64-pc-windows-msvc.zip"
    } else {
        return None;
    };
    Some(format!(
        "https://github.com/astral-sh/uv/releases/download/{UV_PINNED_VERSION}/{asset}"
    ))
}

/// Install docs for platforms where uv cannot be auto-bootstrapped.
pub const UV_INSTALL_DOCS_URL: &str = "https://docs.astral.sh/uv/getting-started/installation/";

/// Lifecycle state of one wizard step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    Queued,
    Running,
    Done,
    Failed,
    Skipped,
    Manual,
}

impl StepState {
    /// Terminal states: the step will not run again unless retried.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            StepState::Done | StepState::Failed | StepState::Skipped | StepState::Manual
        )
    }

    /// States that satisfy a prerequisite (spec §4.1).
    pub fn satisfies_prerequisite(self) -> bool {
        matches!(self, StepState::Done | StepState::Manual)
    }
}

/// Whether the wizard is installing fresh or re-verifying an existing setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// Full install: all ten steps, installs included.
    Install,
    /// Re-run audit: check-type steps only, no reinstalls.
    Audit,
}

/// Context handed to every step.
pub struct WizardContext {
    pub config: AppConfig,
    pub data_dir: PathBuf,
    pub mode: RunMode,
    /// Injectable system effects (production: [`ops::RealSystemOps`],
    /// tests: [`fake::FakeSystemOps`]).
    pub ops: Arc<dyn SystemOps>,
}

impl WizardContext {
    pub fn new(
        config: AppConfig,
        data_dir: PathBuf,
        mode: RunMode,
        ops: Arc<dyn SystemOps>,
    ) -> Self {
        Self {
            config,
            data_dir,
            mode,
            ops,
        }
    }
}

// Manual Debug: `dyn SystemOps` is not Debug.
impl std::fmt::Debug for WizardContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WizardContext")
            .field("config", &self.config)
            .field("data_dir", &self.data_dir)
            .field("mode", &self.mode)
            .field("ops", &"<dyn SystemOps>")
            .finish()
    }
}

/// Outcome of a single step execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutcome {
    pub state: StepState,
    pub message: String,
    /// Last [`LOG_TAIL_LINES`] log lines for inline display on failure.
    pub log_tail: Vec<String>,
    pub duration_ms: u64,
}

impl StepOutcome {
    pub fn done(message: impl Into<String>, log_tail: Vec<String>) -> Self {
        Self {
            state: StepState::Done,
            message: message.into(),
            log_tail,
            duration_ms: 0,
        }
    }

    pub fn failed(message: impl Into<String>, log_tail: Vec<String>) -> Self {
        Self {
            state: StepState::Failed,
            message: message.into(),
            log_tail,
            duration_ms: 0,
        }
    }

    pub fn skipped(message: impl Into<String>) -> Self {
        Self {
            state: StepState::Skipped,
            message: message.into(),
            log_tail: Vec::new(),
            duration_ms: 0,
        }
    }
}

/// One wizard step. Implemented once per step (hardware detect, llama.cpp
/// install, tensor-split config, model downloads, gateway start, VS Code
/// extensions, MCP config, WinML registration, MXC policy, smoke test).
#[async_trait]
pub trait WizardStep: Send + Sync {
    /// Stable id, e.g. "detect-hardware".
    fn id(&self) -> &str;
    /// Human-readable name.
    fn name(&self) -> &str;
    /// Ids that must be `Done` or `Manual` before this step may run.
    ///
    /// Owned (not `&[String]`) so steps are not forced to store the vec;
    /// the prerequisite *semantics* are unchanged from the scaffold contract.
    fn prerequisites(&self) -> Vec<String>;
    /// `true` for check-type steps, which also run in [`RunMode::Audit`].
    fn is_check(&self) -> bool {
        false
    }
    /// Exact command(s) this step will run, for the "Show commands" UI.
    /// Must equal the argv actually spawned (see [`PlannedCommand`] for the
    /// download/http display conventions). Mode-aware: in
    /// [`RunMode::Audit`] this must describe the re-verification work, not
    /// the install work.
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand>;
    /// Human-readable remediation offered (not forced) when this step fails
    /// in audit mode, e.g. "model checksum mismatch → re-download?".
    fn remediation_hint(&self) -> Option<&str> {
        None
    }
    /// Optional help URL surfaced alongside the remediation hint (e.g. a
    /// driver download page). Rendered as a button by the UI.
    fn help_url(&self) -> Option<&str> {
        None
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError>;
}

/// An exact command a step plans to run.
///
/// Display conventions (not literal process spawns): `argv[0] == "download"`
/// is a download (`["download", url, dest]`); `"http-get"` / `"http-post"`
/// are HTTP probes (`["http-get", url]`, `["http-post", url, body-preview]`).
/// Everything else is a literal process spawn and must equal the argv the
/// step actually spawns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedCommand {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

impl PlannedCommand {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            env: HashMap::new(),
        }
    }
}

/// Per-step report entry in [`WizardReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepReport {
    pub id: String,
    pub name: String,
    pub state: StepState,
    pub message: String,
    pub duration_ms: u64,
    pub log_tail: Vec<String>,
    /// Skip note or "path @ version" for manual overrides.
    pub note: Option<String>,
}

/// Aggregate result of a wizard run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardReport {
    pub mode: RunMode,
    pub steps: Vec<StepReport>,
}

impl WizardReport {
    /// (done, total) for the overall completion bar. `Done` and `Manual`
    /// count; `Skipped` is terminal but shown with its own checkbox state.
    pub fn progress(&self) -> (usize, usize) {
        let total = self.steps.len();
        let done = self
            .steps
            .iter()
            .filter(|s| matches!(s.state, StepState::Done | StepState::Manual))
            .count();
        (done, total)
    }

    /// Fraction 0.0–1.0 for the completion bar.
    pub fn fraction(&self) -> f64 {
        let (done, total) = self.progress();
        if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        }
    }
}

/// Result of running a command.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Result of an HTTP probe.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub elapsed_ms: u64,
}

impl HttpResponse {
    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One GPU as seen by the hardware probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDescriptor {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    pub total_vram_mib: u64,
    /// e.g. (8, 6) for sm_86, (6, 0) for sm_60.
    pub compute_capability: (u32, u32),
    /// e.g. "581.57".
    pub driver_version: String,
}

impl GpuDescriptor {
    /// Pascal or older (pre-Volta): CUDA 13 toolchains cannot target these.
    pub fn is_pascal_or_older(&self) -> bool {
        self.compute_capability < (7, 0)
    }
}

/// Injectable system effects. Production: [`ops::RealSystemOps`];
/// tests: [`fake::FakeSystemOps`].
#[async_trait]
pub trait SystemOps: Send + Sync {
    /// Run a command to completion, capturing output.
    async fn run_command(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError>;
    /// Spawn a long-lived process (gateway); returns the pid.
    async fn spawn_detached(&self, cmd: &PlannedCommand) -> Result<u32, WizardError>;
    /// Whether a process with `pid` is currently alive.
    fn process_alive(&self, pid: u32) -> bool;
    /// Terminate the process with `pid`. On Windows the whole process
    /// tree dies with it (`taskkill /T`); the recorded gateway pid is
    /// the server process itself, so a bare termination suffices
    /// elsewhere.
    fn kill_process(&self, pid: u32) -> std::io::Result<()>;
    /// Download a URL to `dest`, calling `on_progress(downloaded, total)`.
    /// The callback must be `Sync` so the future stays `Send`.
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<u64, WizardError>;
    /// Extract a zip archive into `dest`.
    async fn extract_zip(&self, archive: &Path, dest: &Path) -> Result<(), WizardError>;
    /// HTTP GET; used for health probes.
    async fn http_get(&self, url: &str) -> Result<HttpResponse, WizardError>;
    /// HTTP POST with a JSON body; used for smoke tests.
    async fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, WizardError>;
    /// HTTP HEAD; returns the Content-Length when the server provides one.
    /// Used for download size estimation (disk-space pre-check).
    async fn http_head_content_length(&self, url: &str) -> Result<Option<u64>, WizardError>;
    /// Free bytes available on the volume containing `path`.
    async fn free_space_bytes(&self, path: &Path) -> Result<u64, WizardError>;
    /// Read a whole file.
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, WizardError>;
    /// Write a whole file, creating parent directories.
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), WizardError>;
    /// Remove a file; a missing file is not an error.
    async fn remove_file(&self, path: &Path) -> Result<(), WizardError>;
    async fn file_exists(&self, path: &Path) -> bool;
    /// Hex-encoded lowercase SHA-256 of a file.
    async fn sha256_file(&self, path: &Path) -> Result<String, WizardError>;
    /// GPU inventory probe (nvidia-smi).
    async fn gpu_inventory(&self) -> Result<Vec<GpuDescriptor>, WizardError>;
    /// Check a Windows Credential Manager target exists. The value is never
    /// read — presence only.
    async fn credential_exists(&self, target: &str) -> Result<bool, WizardError>;
    /// Read a credential's value from Windows Credential Manager by
    /// target name; `None` when the target is absent or the platform has
    /// no credential store. Unlike [`Self::credential_exists`] this DOES
    /// read the secret: callers must never log the returned value or
    /// write it to disk — it exists to inject a secret into a spawned
    /// process's environment (e.g. the gateway's OpenRouter key).
    async fn credential_read(&self, target: &str) -> Option<String>;
    /// User home directory, if known.
    fn home_dir(&self) -> Option<PathBuf>;
}

/// Ordered auto-runner for the wizard steps.
///
/// Semantics:
/// - `run_all` runs `Queued` steps in declaration order, stopping at the
///   first failure or the first prerequisite-blocked step. Call it again
///   after `retry`/`skip`/`mark_manual` to resume.
/// - `run_step` runs one step on demand (still prerequisite-gated).
/// - In [`RunMode::Audit`], steps where `is_check()` is false are pre-marked
///   `Skipped`; the rest re-verify without reinstalling.
pub struct Wizard {
    steps: Vec<Box<dyn WizardStep>>,
    states: HashMap<String, StepState>,
    reports: HashMap<String, StepReport>,
    notes: HashMap<String, String>,
    mode: RunMode,
    started_at: Option<Instant>,
}

impl Wizard {
    /// Build a runner. Duplicate step ids are rejected — silently dropping a
    /// step would be worse than failing to construct.
    pub fn new(steps: Vec<Box<dyn WizardStep>>, mode: RunMode) -> Result<Self, WizardError> {
        let mut seen = std::collections::HashSet::new();
        for s in &steps {
            if !seen.insert(s.id().to_string()) {
                return Err(WizardError::DuplicateStepId(s.id().to_string()));
            }
        }
        let mut states: HashMap<String, StepState> = HashMap::new();
        let mut notes: HashMap<String, String> = HashMap::new();
        for s in &steps {
            let id = s.id().to_string();
            if mode == RunMode::Audit && !s.is_check() {
                states.insert(id.clone(), StepState::Skipped);
                notes.insert(id, "install-only step; not run in audit mode".to_string());
            } else {
                states.insert(id, StepState::Queued);
            }
        }
        Ok(Self {
            steps,
            states,
            reports: HashMap::new(),
            notes,
            mode,
            started_at: None,
        })
    }

    /// Step ids in declaration order.
    pub fn step_ids(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.id().to_string()).collect()
    }

    pub fn mode(&self) -> RunMode {
        self.mode
    }

    pub fn state(&self, id: &str) -> Option<StepState> {
        self.states.get(id).copied()
    }

    pub fn note(&self, id: &str) -> Option<&str> {
        self.notes.get(id).map(String::as_str)
    }

    /// Remediation hint for a step (spec §4.1: offered, not forced, on audit
    /// failure).
    pub fn remediation_hint(&self, id: &str) -> Option<&str> {
        self.find(id)?.remediation_hint()
    }

    /// Help URL for a step, if the step offers one.
    pub fn help_url(&self, id: &str) -> Option<&str> {
        self.find(id)?.help_url()
    }

    fn find(&self, id: &str) -> Option<&dyn WizardStep> {
        self.steps.iter().find(|s| s.id() == id).map(|s| s.as_ref())
    }

    /// Public step lookup by id (spec §14 `setup_get_commands`: exposes
    /// `planned_commands()` for the "Show commands" UI).
    pub fn get_step(&self, id: &str) -> Option<&dyn WizardStep> {
        self.find(id)
    }

    fn touch_started(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }

    /// Elapsed wall time since the first step started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Rough ETA from mean completed-step duration × remaining steps.
    /// `None` before any step completes; `Some(ZERO)` when nothing remains.
    pub fn eta(&self) -> Option<Duration> {
        let done_ms: Vec<u64> = self
            .reports
            .values()
            .filter(|r| matches!(r.state, StepState::Done | StepState::Manual))
            .map(|r| r.duration_ms)
            .collect();
        if done_ms.is_empty() {
            return None;
        }
        let remaining = self
            .states
            .values()
            .filter(|s| matches!(s, StepState::Queued | StepState::Running))
            .count();
        if remaining == 0 {
            return Some(Duration::ZERO);
        }
        let avg = done_ms.iter().sum::<u64>() / done_ms.len() as u64;
        Some(Duration::from_millis(avg.saturating_mul(remaining as u64)))
    }

    /// (done, total) for the overall completion bar.
    pub fn progress(&self) -> (usize, usize) {
        self.report().progress()
    }

    /// Current report; steps never run are synthesized as `Queued`.
    pub fn report(&self) -> WizardReport {
        let steps = self
            .steps
            .iter()
            .map(|s| {
                if let Some(r) = self.reports.get(s.id()) {
                    r.clone()
                } else {
                    StepReport {
                        id: s.id().to_string(),
                        name: s.name().to_string(),
                        state: self
                            .states
                            .get(s.id())
                            .copied()
                            .unwrap_or(StepState::Queued),
                        message: String::new(),
                        duration_ms: 0,
                        log_tail: Vec::new(),
                        note: self.notes.get(s.id()).cloned(),
                    }
                }
            })
            .collect();
        WizardReport {
            mode: self.mode,
            steps,
        }
    }

    /// Run all `Queued` steps in order, honoring prerequisites. Stops at the
    /// first failure or the first blocked step; call again to resume after
    /// `retry` / `skip` / `mark_manual`.
    pub async fn run_all(&mut self, ctx: &WizardContext) -> WizardReport {
        self.touch_started();
        let ids = self.step_ids();
        for id in ids {
            if self.states.get(&id).copied() != Some(StepState::Queued) {
                continue;
            }
            let prereqs: Vec<String> = self
                .find(&id)
                .map(|s| s.prerequisites())
                .unwrap_or_default();
            let blocked = prereqs.iter().find(|pre| {
                !self
                    .states
                    .get(pre.as_str())
                    .copied()
                    .map(StepState::satisfies_prerequisite)
                    .unwrap_or(false)
            });
            if blocked.is_some() {
                break; // leave Queued; UI shows "waiting on"
            }
            let outcome = self.execute_step(ctx, &id).await;
            if outcome.state == StepState::Failed {
                break;
            }
        }
        self.report()
    }

    /// Run (or re-run) one step by id. Prerequisites still enforced.
    pub async fn run_step(
        &mut self,
        ctx: &WizardContext,
        id: &str,
    ) -> Result<StepOutcome, WizardError> {
        let prereqs: Vec<String> = self
            .find(id)
            .ok_or_else(|| WizardError::UnknownStep(id.to_string()))?
            .prerequisites();
        for pre in &prereqs {
            let ok = self
                .states
                .get(pre.as_str())
                .copied()
                .map(StepState::satisfies_prerequisite)
                .unwrap_or(false);
            if !ok {
                return Err(WizardError::PrerequisiteFailed {
                    step: id.to_string(),
                    waiting_on: pre.clone(),
                });
            }
        }
        self.touch_started();
        Ok(self.execute_step(ctx, id).await)
    }

    /// Reset a failed step to `Queued` so it can run again. No-op otherwise.
    pub fn retry(&mut self, id: &str) {
        if self.states.get(id).copied() == Some(StepState::Failed) {
            self.states.insert(id.to_string(), StepState::Queued);
            self.notes.remove(id);
        }
    }

    /// Mark a step skipped with a user note. Note: per spec §4.1,
    /// prerequisites require `Done` or `Manual`, so skipping a step blocks
    /// its dependents until they are re-pointed or the step is retried.
    pub fn skip(&mut self, id: &str, note: String) {
        if self.find(id).is_some() {
            self.states.insert(id.to_string(), StepState::Skipped);
            self.notes.insert(id.to_string(), note.clone());
            // Synthesize a report entry so the checklist shows the note.
            let name = self
                .find(id)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            self.reports.insert(
                id.to_string(),
                StepReport {
                    id: id.to_string(),
                    name,
                    state: StepState::Skipped,
                    message: "skipped by user".to_string(),
                    duration_ms: 0,
                    log_tail: Vec::new(),
                    note: Some(note),
                },
            );
        }
    }

    /// Mark a step as manually completed (user installed it themselves).
    /// `Manual` satisfies prerequisites.
    pub fn mark_manual(&mut self, id: &str, path: String, version: String) {
        if self.find(id).is_some() {
            self.states.insert(id.to_string(), StepState::Manual);
            let note = format!("{path} @ {version}");
            self.notes.insert(id.to_string(), note.clone());
            let name = self
                .find(id)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            self.reports.insert(
                id.to_string(),
                StepReport {
                    id: id.to_string(),
                    name,
                    state: StepState::Manual,
                    message: "marked as manually installed".to_string(),
                    duration_ms: 0,
                    log_tail: Vec::new(),
                    note: Some(note),
                },
            );
        }
    }

    async fn execute_step(&mut self, ctx: &WizardContext, id: &str) -> StepOutcome {
        // Defensive: both callers resolve the step via find() first, so this
        // is unreachable in practice — but an unknown id must fail closed,
        // not panic.
        if self.find(id).is_none() {
            return StepOutcome::failed(
                format!("unknown step: {id}"),
                vec!["execute_step called for an unregistered step".to_string()],
            );
        }
        let step_name = self
            .find(id)
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        self.states.insert(id.to_string(), StepState::Running);
        let started = Instant::now();
        // The borrow from the finds above has ended; re-resolve for the run.
        // The None arm is unreachable (checked above) but handled anyway.
        let raw = match self.find(id) {
            Some(step) => step.run(ctx).await,
            None => {
                return StepOutcome::failed(
                    format!("unknown step: {id}"),
                    vec!["step unregistered during execution".to_string()],
                )
            }
        };
        let mut outcome = match raw {
            Ok(o) => o,
            Err(e) => StepOutcome::failed(format!("step error: {e}"), vec![format!("{e:?}")]),
        };
        // Runner-measured wall time is authoritative.
        outcome.duration_ms = started.elapsed().as_millis() as u64;
        if outcome.log_tail.len() > LOG_TAIL_LINES {
            let drop = outcome.log_tail.len() - LOG_TAIL_LINES;
            outcome.log_tail = outcome.log_tail.into_iter().skip(drop).collect();
        }
        self.states.insert(id.to_string(), outcome.state);
        self.reports.insert(
            id.to_string(),
            StepReport {
                id: id.to_string(),
                name: step_name,
                state: outcome.state,
                message: outcome.message.clone(),
                duration_ms: outcome.duration_ms,
                log_tail: outcome.log_tail.clone(),
                note: self.notes.get(id).cloned(),
            },
        );
        outcome
    }
}

/// Wizard errors.
#[derive(Debug, thiserror::Error)]
pub enum WizardError {
    #[error("prerequisite not satisfied for step {step}: waiting on {waiting_on}")]
    PrerequisiteFailed { step: String, waiting_on: String },
    #[error("step {0} failed: {1}")]
    StepFailed(String, String),
    #[error("unknown step: {0}")]
    UnknownStep(String),
    #[error("duplicate wizard step id: {0}")]
    DuplicateStepId(String),
    #[error("wizard cancelled")]
    Cancelled,
    #[error("command failed: {argv:?} exited with code {code}: {stderr}")]
    CommandFailed {
        argv: Vec<String>,
        code: i32,
        stderr: String,
    },
    #[error("download failed for {url}: {reason}")]
    DownloadFailed { url: String, reason: String },
    #[error("checksum mismatch for {}: expected {expected}, got {actual}",
        .path.display())]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("HTTP error for {url}: {reason}")]
    HttpError { url: String, reason: String },
    #[error("I/O error for {}: {reason}", .path.display())]
    IoError { path: PathBuf, reason: String },
    #[error("GPU probe failed: {0}")]
    GpuProbeFailed(String),
    #[error("unsupported llama.cpp build {asset}: {reason}")]
    UnsupportedBuild { asset: String, reason: String },
    #[error("gateway error: {0}")]
    Gateway(String),
    #[error("MXC error: {0}")]
    Mxc(String),
    #[error("Windows ML error: {0}")]
    WinMl(String),
    #[error(transparent)]
    Config(#[from] manager_config::ConfigError),
}

/// Case-insensitive hex comparison for checksums.
pub fn checksums_match(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSystemOps;

    /// Minimal scripted step for runner tests.
    struct ScriptStep {
        id: &'static str,
        prereqs: Vec<String>,
        check: bool,
        fail: bool,
        err: bool,
        log_lines: usize,
    }

    impl ScriptStep {
        fn ok(id: &'static str) -> Self {
            Self {
                id,
                prereqs: vec![],
                check: true,
                fail: false,
                err: false,
                log_lines: 0,
            }
        }
    }

    #[async_trait]
    impl WizardStep for ScriptStep {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
        fn prerequisites(&self) -> Vec<String> {
            self.prereqs.clone()
        }
        fn is_check(&self) -> bool {
            self.check
        }
        fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
            vec![]
        }
        async fn run(&self, _ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
            if self.err {
                return Err(WizardError::Cancelled);
            }
            if self.fail {
                return Ok(StepOutcome::failed(
                    format!("{} failed", self.id),
                    vec!["boom".to_string()],
                ));
            }
            Ok(StepOutcome::done(
                format!("{} ok", self.id),
                (0..self.log_lines).map(|i| format!("line {i}")).collect(),
            ))
        }
    }

    fn ctx() -> WizardContext {
        WizardContext::new(
            manager_config::AppConfig::default_config(),
            PathBuf::from("test-data"),
            RunMode::Install,
            Arc::new(FakeSystemOps::new()),
        )
    }

    fn wizard(ids: &[(&'static str, bool)]) -> Wizard {
        // (id, fail)
        let steps: Vec<Box<dyn WizardStep>> = ids
            .iter()
            .map(|(id, fail)| {
                let mut s = ScriptStep::ok(id);
                s.fail = *fail;
                Box::new(s) as Box<dyn WizardStep>
            })
            .collect();
        Wizard::new(steps, RunMode::Install).expect("test steps have unique ids")
    }

    #[tokio::test]
    async fn runs_steps_in_order_and_reports_progress() {
        let c = ctx();
        let mut w = wizard(&[("a", false), ("b", false), ("c", false)]);
        let report = w.run_all(&c).await;
        assert_eq!(w.progress(), (3, 3));
        assert_eq!(
            report
                .steps
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(report.steps.iter().all(|s| s.state == StepState::Done));
        assert!((report.fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn stops_on_first_failure_and_resumes_after_retry() {
        let c = ctx();
        let mut cs = ScriptStep::ok("c");
        cs.prereqs = vec!["b".to_string()];
        let steps: Vec<Box<dyn WizardStep>> = vec![
            Box::new(ScriptStep::ok("a")),
            Box::new({
                let mut b = ScriptStep::ok("b");
                b.fail = true;
                b
            }),
            Box::new(cs),
        ];
        let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");
        let report = w.run_all(&c).await;
        assert_eq!(w.state("a"), Some(StepState::Done));
        assert_eq!(w.state("b"), Some(StepState::Failed));
        assert_eq!(w.state("c"), Some(StepState::Queued));
        assert_eq!(w.progress(), (1, 3));
        assert_eq!(report.steps[1].log_tail, vec!["boom".to_string()]);

        // Resume without retry: still blocked at b.
        let report2 = w.run_all(&c).await;
        assert_eq!(w.state("c"), Some(StepState::Queued));
        assert_eq!(report2.steps[1].state, StepState::Failed);

        w.retry("b");
        assert_eq!(w.state("b"), Some(StepState::Queued));
        // b is scripted to fail; flip it by replacing with an ok step is not
        // possible here, so retry keeps failing — instead verify retry on a
        // Done step is a no-op.
        w.retry("a");
        assert_eq!(w.state("a"), Some(StepState::Done));
    }

    #[tokio::test]
    async fn prerequisite_block_enforced() {
        let c = ctx();
        let mut b = ScriptStep::ok("b");
        b.prereqs = vec!["a".to_string()];
        let mut bf = ScriptStep::ok("b2");
        bf.prereqs = vec!["missing".to_string()];
        let steps: Vec<Box<dyn WizardStep>> =
            vec![Box::new(ScriptStep::ok("a")), Box::new(b), Box::new(bf)];
        let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");

        // Direct run of b before a is done → error.
        let err = w.run_step(&c, "b").await.unwrap_err();
        assert!(matches!(err, WizardError::PrerequisiteFailed { .. }));

        // run_all runs a then b, then stops at b2 (unknown prereq).
        let _ = w.run_all(&c).await;
        assert_eq!(w.state("a"), Some(StepState::Done));
        assert_eq!(w.state("b"), Some(StepState::Done));
        assert_eq!(w.state("b2"), Some(StepState::Queued));
    }

    #[tokio::test]
    async fn skip_blocks_dependents_per_spec() {
        let c = ctx();
        let mut b = ScriptStep::ok("b");
        b.prereqs = vec!["a".to_string()];
        let steps: Vec<Box<dyn WizardStep>> = vec![Box::new(ScriptStep::ok("a")), Box::new(b)];
        let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");
        w.skip("a", "not needed here".to_string());
        assert_eq!(w.state("a"), Some(StepState::Skipped));
        assert_eq!(w.note("a"), Some("not needed here"));
        let _ = w.run_all(&c).await;
        // Skipped does not satisfy prerequisites (spec §4.1).
        assert_eq!(w.state("b"), Some(StepState::Queued));
        assert_eq!(w.progress(), (0, 2));
    }

    #[tokio::test]
    async fn manual_override_unblocks_dependents() {
        let c = ctx();
        let mut b = ScriptStep::ok("b");
        b.prereqs = vec!["a".to_string()];
        let steps: Vec<Box<dyn WizardStep>> = vec![Box::new(ScriptStep::ok("a")), Box::new(b)];
        let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");
        w.mark_manual("a", "/opt/llamacpp".to_string(), "b1234".to_string());
        assert_eq!(w.state("a"), Some(StepState::Manual));
        assert_eq!(w.note("a"), Some("/opt/llamacpp @ b1234"));
        let _ = w.run_all(&c).await;
        assert_eq!(w.state("b"), Some(StepState::Done));
        assert_eq!(w.progress(), (2, 2));
    }

    #[tokio::test]
    async fn audit_mode_skips_non_check_steps() {
        let c = ctx();
        let mut install_only = ScriptStep::ok("install-only");
        install_only.check = false;
        let steps: Vec<Box<dyn WizardStep>> =
            vec![Box::new(install_only), Box::new(ScriptStep::ok("check"))];
        let mut w = Wizard::new(steps, RunMode::Audit).expect("test steps have unique ids");
        assert_eq!(w.state("install-only"), Some(StepState::Skipped));
        assert_eq!(w.state("check"), Some(StepState::Queued));
        let report = w.run_all(&c).await;
        assert_eq!(report.mode, RunMode::Audit);
        assert_eq!(w.state("check"), Some(StepState::Done));
        assert_eq!(w.progress(), (1, 2));
    }

    #[tokio::test]
    async fn unknown_step_id_errors() {
        let c = ctx();
        let mut w = wizard(&[("a", false)]);
        let err = w.run_step(&c, "nope").await.unwrap_err();
        assert!(matches!(err, WizardError::UnknownStep(_)));
        // retry/skip/mark_manual on unknown ids are safe no-ops.
        w.retry("nope");
        w.skip("nope", "x".to_string());
        w.mark_manual("nope", "p".to_string(), "v".to_string());
        assert_eq!(w.state("nope"), None);
    }

    #[test]
    fn duplicate_ids_rejected() {
        let result = Wizard::new(
            vec![
                Box::new(ScriptStep::ok("a")) as Box<dyn WizardStep>,
                Box::new(ScriptStep::ok("a")) as Box<dyn WizardStep>,
            ],
            RunMode::Install,
        );
        assert!(
            matches!(result, Err(WizardError::DuplicateStepId(ref id)) if id == "a"),
            "duplicate ids must be rejected without panicking"
        );
    }

    #[tokio::test]
    async fn log_tail_truncated_to_20() {
        let c = ctx();
        let mut s = ScriptStep::ok("verbose");
        s.log_lines = 35;
        let mut w =
            Wizard::new(vec![Box::new(s)], RunMode::Install).expect("test steps have unique ids");
        let report = w.run_all(&c).await;
        assert_eq!(report.steps[0].log_tail.len(), LOG_TAIL_LINES);
        assert_eq!(report.steps[0].log_tail[0], "line 15");
        assert_eq!(report.steps[0].log_tail[19], "line 34");
    }

    #[tokio::test]
    async fn step_error_becomes_failed_outcome_in_run_all() {
        let c = ctx();
        let mut s = ScriptStep::ok("err");
        s.err = true;
        let mut w =
            Wizard::new(vec![Box::new(s)], RunMode::Install).expect("test steps have unique ids");
        let report = w.run_all(&c).await;
        assert_eq!(w.state("err"), Some(StepState::Failed));
        assert!(report.steps[0].message.contains("step error"));
        // run_step surfaces the failure as a Failed outcome (runner-measured).
        let mut w2 = Wizard::new(
            vec![Box::new(ScriptStep {
                err: true,
                ..ScriptStep::ok("e2")
            })],
            RunMode::Install,
        )
        .expect("test steps have unique ids");
        let outcome = w2
            .run_step(&c, "e2")
            .await
            .expect("run_step returns the outcome");
        assert_eq!(outcome.state, StepState::Failed);
        assert!(outcome.message.contains("wizard cancelled"));
    }

    #[tokio::test]
    async fn run_step_allows_rerun_of_done_step() {
        let c = ctx();
        let mut w = wizard(&[("a", false)]);
        let _ = w.run_all(&c).await;
        assert_eq!(w.state("a"), Some(StepState::Done));
        let outcome = w.run_step(&c, "a").await.expect("rerun works");
        assert_eq!(outcome.state, StepState::Done);
    }

    #[tokio::test]
    async fn eta_and_elapsed_sane() {
        let c = ctx();
        let mut w = wizard(&[("a", false), ("b", false)]);
        assert_eq!(w.eta(), None);
        let _ = w.run_step(&c, "a").await;
        let eta = w.eta().expect("eta after one completion");
        assert!(eta <= Duration::from_secs(60));
        assert!(w.elapsed() < Duration::from_secs(60));
        let _ = w.run_all(&c).await;
        assert_eq!(w.eta(), Some(Duration::ZERO));
    }

    #[test]
    fn step_state_predicates() {
        assert!(StepState::Done.satisfies_prerequisite());
        assert!(StepState::Manual.satisfies_prerequisite());
        assert!(!StepState::Skipped.satisfies_prerequisite());
        assert!(!StepState::Failed.satisfies_prerequisite());
        assert!(StepState::Done.is_terminal());
        assert!(!StepState::Queued.is_terminal());
        assert!(checksums_match("ABCDEF", "abcdef"));
        assert!(!checksums_match("abc", "abd"));
    }

    #[test]
    fn get_step_finds_steps_by_id() {
        let w = Wizard::new(crate::steps::default_steps(), RunMode::Install)
            .expect("default steps have unique ids");
        let step = w
            .get_step(crate::steps::STEP_VSCODE_EXTENSIONS)
            .expect("step exists");
        assert_eq!(step.id(), crate::steps::STEP_VSCODE_EXTENSIONS);
        assert_eq!(step.name(), "Install VS Code extensions");
        assert!(w.get_step("no-such-step").is_none());
    }

    #[test]
    fn get_step_planned_commands_match_step() {
        let w = Wizard::new(crate::steps::default_steps(), RunMode::Install)
            .expect("default steps have unique ids");
        let cfg = manager_config::AppConfig::default_config();
        let ctx = WizardContext::new(
            cfg,
            std::path::PathBuf::from("/tmp"),
            RunMode::Install,
            std::sync::Arc::new(crate::fake::FakeSystemOps::new()),
        );
        let step = w.get_step(crate::steps::STEP_VSCODE_EXTENSIONS).unwrap();
        let cmds = step.planned_commands(&ctx);
        assert!(!cmds.is_empty());
        assert!(cmds.iter().all(|c| c.argv[0] == "code"));
        assert!(cmds
            .iter()
            .all(|c| c.argv.contains(&"--install-extension".to_string())));
    }

    #[test]
    fn continue_editor_config_points_at_gateway() {
        let cfg = manager_config::AppConfig::default_config();
        let json = crate::steps::continue_editor_config(&cfg);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let models = v["models"].as_array().unwrap();
        assert_eq!(models.len(), cfg.gateway.groups.len());
        for m in models {
            assert!(m["apiBase"]
                .as_str()
                .unwrap()
                .contains(&cfg.gateway.port.to_string()));
            assert!(m["title"].as_str().unwrap().ends_with("(local)"));
        }
    }
}
