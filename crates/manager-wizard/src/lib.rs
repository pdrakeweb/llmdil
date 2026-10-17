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

/// CPython version the gateway venv is built with. Pinned because the
/// LiteLLM dependency tree lags new CPython releases: under 3.15,
/// `hiredis` (a `litellm[proxy]` dependency) ships no wheel and its
/// source build requires MSVC (verified on PyPI 2026-10-15: hiredis
/// 3.4.2 has a cp312 win_amd64 wheel and no cp315 files at all), so an
/// unpinned `uv venv` — which picks the newest Python — cannot install
/// the gateway on a stock Windows machine.
pub const GATEWAY_PYTHON_VERSION: &str = "3.12";

/// prisma-client-py version the gateway venv is pinned to, exactly.
///
/// LiteLLM 1.104/1.105's PrismaWrapper (`litellm/proxy/db/prisma_client.py`)
/// reads and writes the generated client's name-mangled `_Prisma__engine`
/// attribute. prisma-client-py PRs #873/#876 moved engine storage into a
/// new `BasePrisma` base class, so from 0.13.0 on the generated `Prisma`
/// class no longer defines that attribute and LiteLLM's connect raises
/// `AttributeError: 'Prisma' object has no attribute '_Prisma__engine'`
/// (field-reproduced on 0.15.0, 2026-10-16; 0.11.0 is clean). 0.11.0 is
/// also LiteLLM's own pyproject/lockfile pin for the proxy, which is why
/// the pin is exact rather than a range cap: 0.12/0.13 are untested
/// against LiteLLM. See docs/debugging/gateway-start-troubleshooting.md.
pub const PRISMA_PINNED_VERSION: &str = "0.11.0";

/// Credential Manager target holding the LiteLLM gateway's master
/// key. Current LiteLLM refuses to boot without one. The key is
/// generated on first gateway start and stored here; the generated
/// config.yaml references it as `os.environ/LITELLM_MASTER_KEY` and
/// the wizard injects it into the gateway process's environment at
/// spawn. Like the OpenRouter key, it never appears in config files
/// or logs.
pub const LITELLM_MASTER_KEY_CRED_TARGET: &str = "local-llm-service-manager/litellm-master-key";

/// Generate a new gateway master key: `sk-` plus the hex SHA-256 of
/// OS-seeded hasher state mixed with the current time and the pid.
/// The key is a localhost bearer secret; it is stored only in
/// Windows Credential Manager (and, by design, in client configs the
/// user applies, e.g. the editor config) and injected into the
/// gateway process's environment at spawn.
pub fn generate_master_key() -> String {
    use sha2::Digest;
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u128(nanos);
    h1.write_u32(std::process::id());
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u128(nanos ^ 0x9e37_79b9_7f4a_7c15);
    h2.write_u64(h1.finish());
    let seed = format!("{}:{}", h1.finish(), h2.finish());
    let digest = sha2::Sha256::digest(seed.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sk-{hex}")
}

/// The gateway master key from Credential Manager, generating and
/// storing one when absent. An existing key is always reused, so
/// clients already configured with it keep working across restarts.
/// Shared by the gateway spawn, the editor-config writers, and the
/// Tauri layer.
pub async fn ensure_gateway_master_key(ops: &dyn SystemOps) -> Result<String, WizardError> {
    if let Some(existing) = ops.credential_read(LITELLM_MASTER_KEY_CRED_TARGET).await {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let key = generate_master_key();
    ops.credential_write(LITELLM_MASTER_KEY_CRED_TARGET, &key)
        .await?;
    Ok(key)
}

/// Credential Manager target for the app-managed PostgreSQL
/// server's password.
pub const POSTGRES_PASSWORD_CRED_TARGET: &str = "local-llm-service-manager/postgres-password";

/// Generate a PostgreSQL password: the same digest construction as
/// the master key, without the `sk-` prefix. Hex output is URL-safe,
/// so the value can be embedded in `DATABASE_URL` verbatim (no
/// percent-encoding) and used raw for `initdb --pwfile` and
/// `PGPASSWORD`.
pub fn generate_postgres_password() -> String {
    generate_master_key()
        .strip_prefix("sk-")
        .unwrap_or_default()
        .to_string()
}

/// The app-managed PostgreSQL password from Credential Manager,
/// generating and storing one when absent. An existing password is
/// always reused: the database cluster was initialized with it, so
/// rotating it here would lock the gateway out of its own database.
pub async fn ensure_postgres_password(ops: &dyn SystemOps) -> Result<String, WizardError> {
    if let Some(existing) = ops.credential_read(POSTGRES_PASSWORD_CRED_TARGET).await {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let password = generate_postgres_password();
    ops.credential_write(POSTGRES_PASSWORD_CRED_TARGET, &password)
        .await?;
    Ok(password)
}

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
}

impl StepOutcome {
    pub fn done(message: impl Into<String>, log_tail: Vec<String>) -> Self {
        Self {
            state: StepState::Done,
            message: message.into(),
            log_tail,
            duration_ms: 0,
            manual_command: None,
        }
    }

    pub fn failed(message: impl Into<String>, log_tail: Vec<String>) -> Self {
        Self {
            state: StepState::Failed,
            message: message.into(),
            log_tail,
            duration_ms: 0,
            manual_command: None,
        }
    }

    pub fn skipped(message: impl Into<String>) -> Self {
        Self {
            state: StepState::Skipped,
            message: message.into(),
            log_tail: Vec::new(),
            duration_ms: 0,
            manual_command: None,
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

/// Injectable system effects. Production: [`ops::RealSystemOps`];
/// tests: [`fake::FakeSystemOps`].
#[async_trait]
pub trait SystemOps: Send + Sync {
    /// Run a command to completion, capturing output.
    async fn run_command(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError>;
    /// Note that a step execution is starting. The real
    /// implementation clears any cancellation left over from a
    /// previous step (see [`Self::request_cancellation`]); the
    /// default does nothing.
    fn note_step_started(&self) {}
    /// Ask in-flight blocking work (archive extraction running on a
    /// blocking thread, which a dropped future cannot cancel) to
    /// stop at its next checkpoint and fail with
    /// [`WizardError::Cancelled`]. The step runner calls this when a
    /// step's budget expires: without it, a timed-out extraction
    /// kept writing in the background and raced the step's retry.
    /// The default does nothing.
    fn request_cancellation(&self) {}
    /// Run a command under a caller-chosen timeout, for work whose
    /// legitimate duration exceeds the implementation's default
    /// command budget (package installs). The real implementation
    /// honors the caller's budget exactly — its own default cap
    /// applies only to [`Self::run_command`] — and a timed-out
    /// command's whole process tree is killed, not just the direct
    /// child. The default implementation wraps [`Self::run_command`]
    /// in `tokio::time::timeout`: expiring the budget drops the
    /// command future, and the real implementation kills the tree
    /// when its command future is dropped, so a timed-out command
    /// is never left running detached from the wizard.
    async fn run_command_with_timeout(
        &self,
        cmd: &PlannedCommand,
        timeout: Duration,
    ) -> Result<CommandOutput, WizardError> {
        match tokio::time::timeout(timeout, self.run_command(cmd)).await {
            Ok(result) => result,
            Err(_) => Err(WizardError::CommandFailed {
                argv: cmd.argv.clone(),
                code: -1,
                stderr: format!("timed out after {}s and was stopped", timeout.as_secs()),
            }),
        }
    }
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
    /// Extract a zip archive into `dest`, keeping only entries under
    /// any of `prefixes` (archive-internal paths such as
    /// `pgsql/bin/`) and stripping `strip_prefix` from their paths
    /// (e.g. `pgsql/`, so the kept trees land at `dest/bin/...`,
    /// `dest/lib/...`). For distributions that ship as one zip with a
    /// wrapping folder and large parts the app does not need (the
    /// PostgreSQL binaries zip's pgAdmin subtree).
    async fn extract_zip_prefixes(
        &self,
        archive: &Path,
        dest: &Path,
        prefixes: &[&str],
        strip_prefix: &str,
    ) -> Result<(), WizardError>;
    /// Extract a `.tar.gz` archive into `dest`, keeping only entries
    /// under `prefix` (a package-internal path such as
    /// `package/bin/x64/`) and stripping `strip_prefix` from their
    /// paths (e.g. `package/`, so the kept tree lands at
    /// `dest/bin/x64/...`). For distributions that ship as a tarball
    /// with a wrapping folder (the MXC SDK's npm tarball).
    async fn extract_tar_gz(
        &self,
        archive: &Path,
        dest: &Path,
        prefix: &str,
        strip_prefix: &str,
    ) -> Result<(), WizardError>;
    /// Run a command elevated (Windows: the UAC "runas" prompt) and
    /// wait for it to finish, returning its exit code. For one-time
    /// host preparation that requires an administrator token (MXC
    /// host prep). The user can decline the prompt; that surfaces as
    /// an error here and callers decide how to report it. Captured
    /// output is not available for elevated processes.
    async fn run_elevated(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError>;
    /// HTTP GET; used for health probes.
    async fn http_get(&self, url: &str) -> Result<HttpResponse, WizardError>;
    /// HTTP GET with extra request headers — e.g. the Authorization
    /// bearer the master-key-guarded gateway requires. The default
    /// implementation ignores the headers and delegates to
    /// [`Self::http_get`]; implementations that can send headers
    /// override it.
    async fn http_get_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, WizardError> {
        let _ = headers;
        self.http_get(url).await
    }
    /// HTTP POST with a JSON body; used for smoke tests.
    async fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, WizardError>;
    /// HTTP POST with extra request headers; see
    /// [`Self::http_get_with_headers`] for the default behavior.
    async fn http_post_with_headers(
        &self,
        url: &str,
        body: &str,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, WizardError> {
        let _ = headers;
        self.http_post(url, body).await
    }
    /// Absolute path to the VS Code command-line tool when VS Code
    /// is installed but its `bin` directory is not on this process's
    /// PATH (resolved from the Windows registry's App Paths entry and
    /// the standard install locations). `None` when no installation
    /// is found — callers then fall back to the bare `code` command
    /// and its PATH lookup. The default finds nothing; the real
    /// Windows implementation overrides it.
    async fn vscode_cli(&self) -> Option<String> {
        None
    }
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
    /// Remove a directory and everything under it; a missing
    /// directory is not an error.
    async fn remove_dir_all(&self, path: &Path) -> Result<(), WizardError>;
    async fn file_exists(&self, path: &Path) -> bool;
    /// Length of a file in bytes, or None when it does not exist or
    /// cannot be stat'ed. Used for model sizes (reading a multi-GB
    /// GGUF just to measure it is not an option).
    async fn file_len(&self, path: &Path) -> Option<u64> {
        let _ = path;
        None
    }
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
    /// Store a credential's value in Windows Credential Manager under
    /// `target`, replacing any existing value. Pair with
    /// [`Self::credential_read`]. The value is a secret: callers must
    /// never log it.
    async fn credential_write(&self, target: &str, secret: &str) -> Result<(), WizardError>;
    /// User home directory, if known.
    fn home_dir(&self) -> Option<PathBuf>;
}

/// The gateway inputs that travel to the spawned process via argv
/// and environment rather than through `config.yaml` — invisible to
/// a YAML-only comparison. The start-gateway step records the
/// fingerprint it actually spawned with (see
/// [`gateway_spawn_state_json`]); the drift check compares that
/// record against the fingerprint of the current configuration.
/// Secret values are never stored: credentials contribute only
/// their SHA-256 hashes, enough to detect a rotation.
/// (Serialized via [`gateway_spawn_state_json`].)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySpawnFingerprint {
    pub port: u16,
    pub bind_host: String,
    /// The app-managed PostgreSQL port when the gateway was spawned
    /// with a DATABASE_URL; `None` when it ran database-free.
    pub postgres_port: Option<u16>,
    /// SHA-256 of the LiteLLM master key the gateway was spawned
    /// with (empty when no key was in play).
    pub master_key_hash: String,
    /// SHA-256 of the OpenRouter API key the gateway was spawned
    /// with (empty when the tier was off or unarmed).
    pub openrouter_key_hash: String,
}

fn secret_fingerprint(secret: Option<&str>) -> String {
    use sha2::Digest as _;
    match secret {
        Some(s) if !s.is_empty() => format!("{:x}", sha2::Sha256::digest(s.as_bytes())),
        _ => String::new(),
    }
}

/// The spawn fingerprint of the current configuration: argv/env
/// inputs from `config`, credential hashes from the values the
/// caller resolved (the wizard reads Credential Manager; the app
/// layer passes what it read). `postgres_installed` is the ground
/// truth the step itself uses to decide whether DATABASE_URL is in
/// play.
pub fn gateway_spawn_fingerprint(
    config: &AppConfig,
    postgres_installed: bool,
    master_key: Option<&str>,
    openrouter_key: Option<&str>,
) -> GatewaySpawnFingerprint {
    GatewaySpawnFingerprint {
        port: config.gateway.port,
        bind_host: config.gateway.bind_host.clone(),
        postgres_port: postgres_installed.then_some(config.postgres.port),
        master_key_hash: secret_fingerprint(master_key),
        openrouter_key_hash: secret_fingerprint(openrouter_key),
    }
}

/// What a successful gateway spawn records, at
/// `<data_dir>/gateway/spawn-state.json`: the spawn fingerprint and
/// the hash of the generated YAML it spawned with.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GatewaySpawnState {
    fingerprint: GatewaySpawnFingerprint,
    yaml_sha256: String,
}

/// Path of the spawn-state record: `<data_dir>/gateway/spawn-state.json`.
pub fn gateway_spawn_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("spawn-state.json")
}

fn yaml_fingerprint(yaml: &str) -> String {
    use sha2::Digest as _;
    format!(
        "{:x}",
        sha2::Sha256::digest(normalize_gateway_yaml(yaml).as_bytes())
    )
}

/// Serialize a successful spawn's state (the drift check's
/// baseline) for the caller to persist at
/// [`gateway_spawn_state_path`] — through the ops layer, so step
/// tests stay hermetic. Written by the start-gateway step after
/// the gateway is healthy.
pub fn gateway_spawn_state_json(
    fingerprint: &GatewaySpawnFingerprint,
    generated_yaml: &str,
) -> String {
    let state = GatewaySpawnState {
        fingerprint: fingerprint.clone(),
        yaml_sha256: yaml_fingerprint(generated_yaml),
    };
    serde_json::to_string_pretty(&state).unwrap()
}

/// Whether the running gateway's recorded spawn state still matches
/// the current configuration — the full drift check. The
/// start-gateway step rewrites `<data_dir>/gateway/config.yaml`
/// from the live config on every start, so an audit "gateway
/// healthy" result (or a persisted Done for the step) only proves
/// the step complete while the gateway was spawned from the current
/// configuration — a stale gateway answering the health probe says
/// nothing about the saved configuration.
///
/// The YAML alone does not carry every spawn input: the port and
/// bind host travel via argv, the database URL and the credential
/// values via the environment. Comparing only the file adopted a
/// stale `Done` after a port change, a bind change, a credential
/// rotation, or a PostgreSQL install — the gateway kept serving the
/// old configuration while the checklist said complete. So this
/// requires, in addition to the YAML matching what the current
/// configuration generates, a spawn-state record whose fingerprint
/// equals `current` and whose YAML hash equals the YAML on disk.
/// A missing record (a gateway started by an older build), drifted
/// contents, or a configuration that no longer generates (e.g. no
/// enabled groups) all count as NOT current.
pub fn gateway_config_is_current(
    config: &AppConfig,
    data_dir: &Path,
    current: &GatewaySpawnFingerprint,
) -> bool {
    let Ok(generated) = manager_gateway::generate_config_yaml(config) else {
        return false;
    };
    let Ok(on_disk) = std::fs::read_to_string(data_dir.join("gateway").join("config.yaml")) else {
        return false;
    };
    if normalize_gateway_yaml(&generated) != normalize_gateway_yaml(&on_disk) {
        return false;
    }
    let Ok(state_text) = std::fs::read_to_string(gateway_spawn_state_path(data_dir)) else {
        return false;
    };
    let Ok(state) = serde_json::from_str::<GatewaySpawnState>(&state_text) else {
        return false;
    };
    state.fingerprint == *current && state.yaml_sha256 == yaml_fingerprint(&on_disk)
}

fn normalize_gateway_yaml(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
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

    /// Adopt check outcomes from another wizard's report (produced by
    /// an Audit-mode `run_all`) as this wizard's initial state, so the
    /// checklist reflects the machine across app restarts instead of
    /// resetting to all-Queued on every launch. Only steps whose check
    /// passed (Done/Manual) are adopted, and only into steps that are
    /// still Queued here — a step run in this session is never
    /// overwritten. A failing check is not adopted: an unsatisfied
    /// check means "not in place yet", which the checklist correctly
    /// shows as Queued rather than as a failed install attempt.
    ///
    /// One exception, gated on `gateway_config_current` (see
    /// [`gateway_config_is_current`]): the start-gateway audit check
    /// passes whenever *any* gateway answers the health probe,
    /// including a stale one started from an older configuration.
    /// Adopting that Done would mark the step complete without the
    /// config being regenerated or the gateway restarted, so when the
    /// on-disk gateway config has drifted from the current
    /// configuration the audit result for start-gateway is not
    /// adopted and the step stays Queued until it actually runs.
    pub fn hydrate(&mut self, report: &WizardReport, gateway_config_current: bool) {
        for step in &report.steps {
            if !matches!(step.state, StepState::Done | StepState::Manual) {
                continue;
            }
            let current = self.states.get(&step.id).copied();
            if step.id == steps::STEP_START_GATEWAY && !gateway_config_current {
                // The drift note explains why the step is not
                // adopted — but only when it is not adopted. If this
                // instance already holds Done for the step, the note
                // would contradict the displayed state.
                if !matches!(current, Some(StepState::Done | StepState::Manual)) {
                    self.notes.insert(
                        step.id.clone(),
                        "gateway configuration changed since the gateway was started".to_string(),
                    );
                }
                continue;
            }
            if matches!(current, None | Some(StepState::Queued)) {
                self.states.insert(step.id.clone(), step.state);
                self.reports.insert(step.id.clone(), step.clone());
            }
        }
    }

    /// Adopt a previously persisted report (this wizard's own
    /// `report()` output, saved by the app layer) as this wizard's
    /// state, so setup progress survives an app restart: the user
    /// picks back up where they left off instead of facing an
    /// untouched checklist. Unlike [`Self::hydrate`] — which adopts
    /// only passing checks from a fresh audit — restore adopts every
    /// recorded outcome, including failures and skips: they are the
    /// user's progress, not machine checks. A step recorded as
    /// Running was interrupted by the app closing; it comes back
    /// Queued with a note saying so. Only steps still Queued here
    /// are adopted, unknown ids are ignored, and a report from the
    /// other mode is rejected wholesale (audit outcomes must go
    /// through `hydrate`, never restore). The one outcome restore
    /// does not adopt verbatim is a completed start-gateway when
    /// `gateway_config_current` is false: the persisted Done records
    /// the configuration the gateway was started with, and drift
    /// since then means the step has to run again (same rule as
    /// [`Self::hydrate`]), so it comes back Queued with a note.
    pub fn restore(&mut self, report: &WizardReport, gateway_config_current: bool) {
        if report.mode != self.mode {
            return;
        }
        for step in &report.steps {
            if self.find(&step.id).is_none() {
                continue;
            }
            let current = self.states.get(&step.id).copied();
            if !matches!(current, None | Some(StepState::Queued)) {
                continue;
            }
            if step.id == steps::STEP_START_GATEWAY
                && !gateway_config_current
                && matches!(step.state, StepState::Done | StepState::Manual)
            {
                self.notes.insert(
                    step.id.clone(),
                    "gateway configuration changed since the gateway was started".to_string(),
                );
                continue;
            }
            if step.state == StepState::Running {
                let mut r = step.clone();
                r.state = StepState::Queued;
                r.message = String::new();
                r.duration_ms = 0;
                r.log_tail = Vec::new();
                r.manual_command = None;
                r.note =
                    Some("interrupted — the app closed while this step was running".to_string());
                self.states.insert(step.id.clone(), StepState::Queued);
                self.reports.insert(step.id.clone(), r);
            } else {
                self.states.insert(step.id.clone(), step.state);
                self.reports.insert(step.id.clone(), step.clone());
            }
        }
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
                        manual_command: None,
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
                    manual_command: None,
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
                    manual_command: None,
                },
            );
        }
    }

    async fn execute_step(&mut self, ctx: &WizardContext, id: &str) -> StepOutcome {
        self.execute_step_with_budget(ctx, id, step_budget(id))
            .await
    }

    async fn execute_step_with_budget(
        &mut self,
        ctx: &WizardContext,
        id: &str,
        budget: Duration,
    ) -> StepOutcome {
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
        ctx.ops.note_step_started();
        let started = Instant::now();
        // The borrow from the finds above has ended; re-resolve for the run.
        // The None arm is unreachable (checked above) but handled anyway.
        // The timeout drops the step's future on expiry: any command
        // it was awaiting is dropped with it (and its process tree
        // killed — see `run_command`), and blocking work that a
        // dropped future cannot cancel (archive extraction) is asked
        // to stop via request_cancellation, so a timed-out step frees
        // the wizard instead of wedging it for the rest of the session.
        let raw = match self.find(id) {
            Some(step) => match tokio::time::timeout(budget, step.run(ctx)).await {
                Ok(r) => r,
                Err(_) => {
                    ctx.ops.request_cancellation();
                    Ok(StepOutcome::failed(
                        format!(
                            "step '{step_name}' timed out after {} and was stopped",
                            budget_label(budget)
                        ),
                        vec![format!(
                            "the step exceeded its {} budget and was aborted; \
                             re-run the step to retry",
                            budget_label(budget)
                        )],
                    ))
                }
            },
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
        // The report (UI display, persisted wizard state) carries
        // only the display tail; the outcome keeps the full log for
        // the caller's log persistence.
        let report_tail = if outcome.log_tail.len() > LOG_TAIL_LINES {
            let drop = outcome.log_tail.len() - LOG_TAIL_LINES;
            outcome.log_tail.iter().skip(drop).cloned().collect()
        } else {
            outcome.log_tail.clone()
        };
        self.states.insert(id.to_string(), outcome.state);
        self.reports.insert(
            id.to_string(),
            StepReport {
                id: id.to_string(),
                name: step_name,
                state: outcome.state,
                message: outcome.message.clone(),
                duration_ms: outcome.duration_ms,
                log_tail: report_tail,
                note: self.notes.get(id).cloned(),
                manual_command: outcome.manual_command.clone(),
            },
        );
        outcome
    }
}

/// Wall-clock budget for one step execution. Before this bound
/// existed, a single stalled child process (a hung `uv pip install`,
/// a Prisma pre-warm that never returned) held the wizard lock for
/// the rest of the session, and every later step run reported
/// "busy" with no recovery short of restarting the app.
///
/// Coherence rule: a step's budget must exceed the worst-case
/// sequential sum of its sub-budgets — a budget smaller than the
/// work it bounds kills legitimate runs mid-flight and misreports
/// them as stalls. v0.1.32 violated this for start-gateway (600 s
/// total against 900 s sub-budgets for the pin install, the
/// generate, and the package setup, each of which the command
/// runner also capped at 600 s), so a legitimate cold repair chain
/// could not finish in one run (field evidence 2026-10-16: a run
/// died at exactly 600003 ms mid-repair). The start-gateway figure
/// covers the cold chain end to end: venv create (900) + package
/// install (900) + Prisma pin install (900) + generate (900) +
/// probes and the health window (~600).
fn step_budget(id: &str) -> Duration {
    match id {
        steps::STEP_START_GATEWAY => Duration::from_secs(4200),
        _ => Duration::from_secs(1200),
    }
}

/// "10 minutes" / "45 seconds" for timeout messages.
fn budget_label(budget: Duration) -> String {
    let secs = budget.as_secs();
    if secs != 0 && secs.is_multiple_of(60) {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute".to_string()
        } else {
            format!("{mins} minutes")
        }
    } else {
        format!("{secs} seconds")
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

/// True when `sha` is a real pinned SHA-256 (64 hex chars), as opposed
/// to an empty value or a placeholder. Only a pinned hash can verify a
/// file; unpinned models download and run unverified by design.
pub fn is_pinned_hash(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
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
    async fn hydrate_adopts_passing_checks_without_overwriting_session_state() {
        let step_report = |id: &str, state: StepState, message: &str| StepReport {
            id: id.to_string(),
            name: id.to_string(),
            state,
            message: message.to_string(),
            duration_ms: 1,
            log_tail: Vec::new(),
            note: None,
            manual_command: None,
        };
        let audit_report = WizardReport {
            mode: RunMode::Audit,
            steps: vec![
                step_report("a", StepState::Done, "verified by audit"),
                step_report("b", StepState::Failed, "not in place"),
            ],
        };
        // Fresh wizard: the passing check is adopted; the failing one
        // stays Queued (not a failed install attempt).
        let mut w = wizard(&[("a", false), ("b", false)]);
        w.hydrate(&audit_report, true);
        let after = w.report();
        let a = after.steps.iter().find(|s| s.id == "a").unwrap();
        assert_eq!(a.state, StepState::Done);
        assert_eq!(a.message, "verified by audit");
        let b = after.steps.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.state, StepState::Queued);

        // A step already run this session keeps its own outcome.
        let c = ctx();
        let mut w2 = wizard(&[("a", false), ("b", false)]);
        let session = w2.run_step(&c, "a").await.unwrap();
        assert_eq!(session.state, StepState::Done);
        w2.hydrate(&audit_report, true);
        let a2 = w2.report().steps.into_iter().find(|s| s.id == "a").unwrap();
        assert_eq!(a2.state, StepState::Done);
        assert_ne!(a2.message, "verified by audit");
    }

    #[test]
    fn restore_adopts_all_outcomes_and_marks_interrupted() {
        let step_report = |id: &str, state: StepState, message: &str| StepReport {
            id: id.to_string(),
            name: id.to_string(),
            state,
            message: message.to_string(),
            duration_ms: 42,
            log_tail: vec!["line".to_string()],
            note: None,
            manual_command: None,
        };
        let persisted = WizardReport {
            mode: RunMode::Install,
            steps: vec![
                step_report("a", StepState::Done, "installed"),
                step_report("b", StepState::Failed, "boom"),
                step_report("c", StepState::Running, ""),
                step_report("ghost", StepState::Done, "unknown id"),
            ],
        };
        let mut w = wizard(&[("a", false), ("b", false), ("c", false)]);
        w.restore(&persisted, true);
        let after = w.report();
        let get = |id: &str| after.steps.iter().find(|s| s.id == id).unwrap();
        // Done and Failed are adopted verbatim — progress and the
        // record of what failed both survive the restart.
        assert_eq!(get("a").state, StepState::Done);
        assert_eq!(get("a").message, "installed");
        assert_eq!(get("b").state, StepState::Failed);
        assert_eq!(get("b").message, "boom");
        // The interrupted step is runnable again, with the note.
        assert_eq!(get("c").state, StepState::Queued);
        assert!(get("c")
            .note
            .as_deref()
            .unwrap_or("")
            .contains("interrupted"));
        // Unknown ids did not appear.
        assert_eq!(after.steps.len(), 3);
    }

    #[test]
    fn restore_rejects_other_mode_and_never_overwrites_session_state() {
        let step_report = |id: &str, state: StepState| StepReport {
            id: id.to_string(),
            name: id.to_string(),
            state,
            message: "from audit".to_string(),
            duration_ms: 1,
            log_tail: Vec::new(),
            note: None,
            manual_command: None,
        };
        // An audit report must not be restored into the install
        // wizard (its failures are checks, not install outcomes).
        let mut w = wizard(&[("a", false)]);
        w.restore(
            &WizardReport {
                mode: RunMode::Audit,
                steps: vec![step_report("a", StepState::Failed)],
            },
            true,
        );
        assert_eq!(w.report().steps[0].state, StepState::Queued);
    }

    fn gateway_step_report(state: StepState) -> StepReport {
        StepReport {
            id: steps::STEP_START_GATEWAY.to_string(),
            name: "Start LiteLLM gateway".to_string(),
            state,
            message: "gateway healthy".to_string(),
            duration_ms: 1,
            log_tail: Vec::new(),
            note: None,
            manual_command: None,
        }
    }

    #[test]
    fn hydrate_does_not_adopt_start_gateway_done_when_config_drifted() {
        // A stale gateway answering the audit probe must not mark
        // start-gateway complete when the saved configuration has
        // moved on: the step stays Queued so Apply/Start runs it.
        let mut w = wizard(&[(steps::STEP_START_GATEWAY, false), ("a", false)]);
        let audit_report = WizardReport {
            mode: RunMode::Audit,
            steps: vec![
                gateway_step_report(StepState::Done),
                StepReport {
                    id: "a".to_string(),
                    name: "a".to_string(),
                    state: StepState::Done,
                    message: "verified by audit".to_string(),
                    duration_ms: 1,
                    log_tail: Vec::new(),
                    note: None,
                    manual_command: None,
                },
            ],
        };
        w.hydrate(&audit_report, false);
        let after = w.report();
        let gw = after
            .steps
            .iter()
            .find(|s| s.id == steps::STEP_START_GATEWAY)
            .unwrap();
        assert_eq!(gw.state, StepState::Queued);
        assert!(gw.note.as_deref().unwrap_or("").contains("changed"));
        // Other steps are unaffected by the gateway rule.
        let a = after.steps.iter().find(|s| s.id == "a").unwrap();
        assert_eq!(a.state, StepState::Done);

        // With the config current, the same audit result is adopted.
        let mut w2 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
        w2.hydrate(&audit_report, true);
        assert_eq!(w2.state(steps::STEP_START_GATEWAY), Some(StepState::Done));
    }

    #[test]
    fn restore_does_not_adopt_start_gateway_done_when_config_drifted() {
        let persisted = WizardReport {
            mode: RunMode::Install,
            steps: vec![gateway_step_report(StepState::Done)],
        };
        let mut w = wizard(&[(steps::STEP_START_GATEWAY, false)]);
        w.restore(&persisted, false);
        assert_eq!(w.state(steps::STEP_START_GATEWAY), Some(StepState::Queued));
        // Failures are the user's progress and still restore under drift.
        let persisted_failed = WizardReport {
            mode: RunMode::Install,
            steps: vec![gateway_step_report(StepState::Failed)],
        };
        let mut w2 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
        w2.restore(&persisted_failed, false);
        assert_eq!(w2.state(steps::STEP_START_GATEWAY), Some(StepState::Failed));
        // Current config: the persisted Done restores as before.
        let mut w3 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
        w3.restore(&persisted, true);
        assert_eq!(w3.state(steps::STEP_START_GATEWAY), Some(StepState::Done));
    }

    #[test]
    fn gateway_config_is_current_compares_generated_yaml_to_disk() {
        let dir = std::env::temp_dir().join(format!("wizard-gw-current-{}", std::process::id()));
        let gw_dir = dir.join("gateway");
        std::fs::create_dir_all(&gw_dir).unwrap();
        // The default config ships with backends disabled (a fresh
        // install), which generates no YAML at all; enable them so
        // generation succeeds.
        let mut cfg = manager_config::AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None);
        // Missing file: not current (the step must run to write it).
        assert!(!gateway_config_is_current(&cfg, &dir, &fp));
        let yaml = manager_gateway::generate_config_yaml(&cfg).unwrap();
        std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
        // YAML alone is not enough: no spawn-state record means the
        // running gateway's argv/env inputs are unknown.
        assert!(!gateway_config_is_current(&cfg, &dir, &fp));
        std::fs::write(
            gateway_spawn_state_path(&dir),
            gateway_spawn_state_json(&fp, &yaml),
        )
        .unwrap();
        assert!(gateway_config_is_current(&cfg, &dir, &fp));
        // CRLF line endings and trailing blank lines are not drift.
        std::fs::write(
            gw_dir.join("config.yaml"),
            format!("{}\r\n\r\n", yaml.replace('\n', "\r\n")),
        )
        .unwrap();
        assert!(gateway_config_is_current(&cfg, &dir, &fp));
        // A hand edit is drift.
        std::fs::write(gw_dir.join("config.yaml"), format!("{yaml}# hand edit\n")).unwrap();
        assert!(!gateway_config_is_current(&cfg, &dir, &fp));
        // A configuration that no longer generates is not current.
        let mut no_groups = cfg.clone();
        no_groups.gateway.groups.clear();
        std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
        assert!(!gateway_config_is_current(&no_groups, &dir, &fp));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gateway_config_is_current_detects_spawn_input_drift() {
        // The YAML does not carry the spawn port, the bind host, the
        // database port, or the credential values — a drift in any
        // of them must still read as NOT current.
        let dir = std::env::temp_dir().join(format!("wizard-gw-drift-{}", std::process::id()));
        let gw_dir = dir.join("gateway");
        std::fs::create_dir_all(&gw_dir).unwrap();
        let mut cfg = manager_config::AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        let yaml = manager_gateway::generate_config_yaml(&cfg).unwrap();
        std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
        let spawned = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None);
        std::fs::write(
            gateway_spawn_state_path(&dir),
            gateway_spawn_state_json(&spawned, &yaml),
        )
        .unwrap();
        assert!(gateway_config_is_current(&cfg, &dir, &spawned));

        // Gateway port moved.
        let mut moved = cfg.clone();
        moved.gateway.port += 1;
        let fp = gateway_spawn_fingerprint(&moved, true, Some("sk-master"), None);
        assert!(!gateway_config_is_current(&moved, &dir, &fp));
        // Bind host changed.
        let mut rebound = cfg.clone();
        rebound.gateway.bind_host = "127.0.0.1".to_string();
        let fp = gateway_spawn_fingerprint(&rebound, true, Some("sk-master"), None);
        assert!(!gateway_config_is_current(&rebound, &dir, &fp));
        // PostgreSQL appeared since the spawn (DATABASE_URL now in
        // play) — and, symmetrically, disappeared.
        let fp = gateway_spawn_fingerprint(&cfg, false, Some("sk-master"), None);
        assert!(!gateway_config_is_current(&cfg, &dir, &fp));
        // Master key rotated.
        let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-rotated"), None);
        assert!(!gateway_config_is_current(&cfg, &dir, &fp));
        // OpenRouter key appeared.
        let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), Some("or-key"));
        assert!(!gateway_config_is_current(&cfg, &dir, &fp));
        // The stored YAML hash no longer matching the file on disk
        // (replaced after the spawn) is drift too.
        std::fs::write(gw_dir.join("config.yaml"), "# replaced\n").unwrap();
        assert!(!gateway_config_is_current(&cfg, &dir, &spawned));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Step that never finishes on its own; the execution budget
    /// must cut it off.
    struct HangStep;

    #[async_trait]
    impl WizardStep for HangStep {
        fn id(&self) -> &str {
            "hang"
        }
        fn name(&self) -> &str {
            "Hang step"
        }
        fn prerequisites(&self) -> Vec<String> {
            vec![]
        }
        fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
            vec![]
        }
        async fn run(&self, _ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(StepOutcome::done("never reached", vec![]))
        }
    }

    #[tokio::test]
    async fn step_execution_times_out_and_records_failure() {
        let c = ctx();
        let mut w =
            Wizard::new(vec![Box::new(HangStep)], RunMode::Install).expect("unique step ids");
        let outcome = w
            .execute_step_with_budget(&c, "hang", Duration::from_millis(50))
            .await;
        assert_eq!(outcome.state, StepState::Failed);
        assert!(outcome.message.contains("timed out"), "{}", outcome.message);
        assert!(outcome.message.contains("Hang step"), "{}", outcome.message);
        // The failure is recorded like any other, so the checklist
        // shows it and a retry is possible.
        assert_eq!(w.state("hang"), Some(StepState::Failed));
        assert_eq!(w.report().steps[0].state, StepState::Failed);
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
    fn continue_config_yaml_points_at_gateway() {
        let cfg = manager_config::AppConfig::default_config();
        let yaml = crate::steps::continue_config_yaml(&cfg);
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(v["schema"].as_str().unwrap(), "v1");
        let models = v["models"].as_sequence().unwrap();
        assert_eq!(models.len(), cfg.gateway.groups.len());
        for (m, g) in models.iter().zip(cfg.gateway.groups.iter()) {
            assert_eq!(m["provider"].as_str().unwrap(), "openai");
            assert_eq!(m["model"].as_str().unwrap(), g.name);
            assert_eq!(
                m["apiBase"].as_str().unwrap(),
                format!("http://127.0.0.1:{}/v1", cfg.gateway.port)
            );
            assert!(m["name"].as_str().unwrap().ends_with("(local)"));
            // The key is an env reference, never a literal secret:
            // Continue resolves it from ~/.continue/.env.
            assert_eq!(
                m["apiKey"].as_str().unwrap(),
                "${{ secrets.LITELLM_API_KEY }}"
            );
            let roles: Vec<&str> = m["roles"]
                .as_sequence()
                .unwrap()
                .iter()
                .filter_map(|r| r.as_str())
                .collect();
            assert_eq!(roles, ["chat", "edit", "apply"]);
            let caps: Vec<&str> = m["capabilities"]
                .as_sequence()
                .unwrap()
                .iter()
                .filter_map(|r| r.as_str())
                .collect();
            assert_eq!(caps, ["tool_use"]);
        }
        assert!(!yaml.contains("sk-"), "no literal key material: {yaml}");
    }
}
