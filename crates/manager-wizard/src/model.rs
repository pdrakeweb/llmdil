//! Core wizard types: step state, run mode, context, outcomes, the `WizardStep` trait, planned commands, and reports.

use super::*;

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
    /// Full install: all eleven steps, installs included.
    Install,
    /// Re-run audit: check-type steps only, no reinstalls.
    Audit,
}

/// Live sink for step progress lines: invoked with one line at the
/// moment the line is produced, while the step is still running.
/// The step outcome's collected log is otherwise only persisted
/// when the step RETURNS, so without a sink a multi-minute step
/// (a cold package install, a database wait) shows a single
/// "started" line for its whole duration and reads as a hang
/// (field report 2026-10-19: start-gateway silent for minutes at a
/// time). Production attaches a sink that appends to `wizard.log`;
/// tests attach a recorder; steps emit through
/// [`WizardContext::emit_log`] and treat the sink as best-effort —
/// a missing sink changes nothing about the outcome log.
pub type StepLogSink = Arc<dyn Fn(String) + Send + Sync>;

/// Context handed to every step.
pub struct WizardContext {
    pub config: AppConfig,
    pub data_dir: PathBuf,
    pub mode: RunMode,
    /// Injectable system effects (production: [`ops::RealSystemOps`],
    /// tests: [`fake::FakeSystemOps`]).
    pub ops: Arc<dyn SystemOps>,
    /// Live progress sink (see [`StepLogSink`]); `None` outside the
    /// app shell (unit tests, one-shot CLI runs).
    pub log_sink: Option<StepLogSink>,
    /// Who persists the [`ConfigEdit`]s a step makes (see
    /// [`ConfigEditMode`]). The app shell sets
    /// [`ConfigEditMode::ReturnToCaller`] and applies
    /// [`StepOutcome::config_edits`] under its config lock.
    pub config_edit_mode: ConfigEditMode,
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
            log_sink: None,
            config_edit_mode: ConfigEditMode::default(),
        }
    }

    /// Set who persists step config edits (see [`ConfigEditMode`]).
    pub fn with_config_edit_mode(mut self, mode: ConfigEditMode) -> Self {
        self.config_edit_mode = mode;
        self
    }

    /// Attach a live progress sink (see [`StepLogSink`]).
    pub fn with_log_sink(mut self, sink: StepLogSink) -> Self {
        self.log_sink = Some(sink);
        self
    }

    /// The attached live progress sink, when any — for handing to
    /// progress callbacks (download progress) that outlive a single
    /// call frame.
    pub fn log_sink(&self) -> Option<StepLogSink> {
        self.log_sink.clone()
    }

    /// Emit one progress line to the live sink, when one is
    /// attached. Steps call this for progress that must be visible
    /// WHILE the step runs (phase boundaries, wait ticks, streamed
    /// server-log lines); the outcome log remains the durable
    /// end-of-step record.
    pub fn emit_log(&self, line: &str) {
        if let Some(sink) = &self.log_sink {
            sink(line.to_string());
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
    /// The step's full log. The [`StepReport`] the runner derives
    /// from an outcome carries only the last [`LOG_TAIL_LINES`]
    /// lines (inline display); the outcome itself keeps everything,
    /// so the caller can persist the complete log — truncating the
    /// outcome discarded exactly the lines that diagnose a failure
    /// (a start-gateway failure's own pin/probe/generate lines were
    /// displaced by the gateway's banner tail, 2026-10-16).
    pub log_tail: Vec<String>,
    pub duration_ms: u64,
    /// Ready-to-run commands for doing the step's work by hand when
    /// the step itself failed (the start-gateway step sets this: the
    /// exact gateway start commands, secrets named but never valued).
    /// Rendered as a code block under the failure message. Absent on
    /// outcomes that carry no manual path — including every outcome
    /// persisted by older builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
    /// Configuration changes the step made (code-review PS-4; see
    /// [`config_edit`]). Under [`ConfigEditMode::ReturnToCaller`] the
    /// caller applies them under its config lock — on a failed
    /// outcome too (a model verified before a later failure keeps its
    /// stamp). Under [`ConfigEditMode::SaveToDisk`] the step has
    /// already persisted them and they are informational. Absent on
    /// the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_edits: Vec<ConfigEdit>,
}

impl StepOutcome {
    pub fn done(message: impl Into<String>, log_tail: Vec<String>) -> Self {
        Self {
            state: StepState::Done,
            message: message.into(),
            log_tail,
            duration_ms: 0,
            manual_command: None,
            config_edits: Vec::new(),
        }
    }

    pub fn failed(message: impl Into<String>, log_tail: Vec<String>) -> Self {
        Self {
            state: StepState::Failed,
            message: message.into(),
            log_tail,
            duration_ms: 0,
            manual_command: None,
            config_edits: Vec::new(),
        }
    }

    pub fn skipped(message: impl Into<String>) -> Self {
        Self {
            state: StepState::Skipped,
            message: message.into(),
            log_tail: Vec::new(),
            duration_ms: 0,
            manual_command: None,
            config_edits: Vec::new(),
        }
    }

    /// Attach the manual-command block (see
    /// [`StepOutcome::manual_command`]) to this outcome.
    pub fn with_manual_command(mut self, manual_command: impl Into<String>) -> Self {
        self.manual_command = Some(manual_command.into());
        self
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
    /// For detached spawns: files the child's stdout/stderr are
    /// captured to (rotated to `<path>.prev` on each spawn, one
    /// previous run kept). `None` discards the output. A detached
    /// process whose output goes nowhere fails invisibly, so any
    /// long-running service spawn should set these.
    pub stdout_log: Option<PathBuf>,
    pub stderr_log: Option<PathBuf>,
}

impl PlannedCommand {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            env: HashMap::new(),
            stdout_log: None,
            stderr_log: None,
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
    /// Manual-command block carried over from the step outcome (see
    /// [`StepOutcome::manual_command`]). Persisted with the report so
    /// a failure's manual path survives an app restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
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
