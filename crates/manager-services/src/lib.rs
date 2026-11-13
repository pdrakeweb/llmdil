//! Windows service supervision (feature branch `feature/windows-services`;
//! see `docs/windows-services-design.md`).
//!
//! Every backend and the gateway can be registered as a Windows
//! service whose ImagePath is the in-repo shim (`llm-service-host`)
//! plus a pointer to a JSON **service definition** in
//! `<data_dir>/services/`. Definitions are generated from the same
//! specs process supervision spawns from ([`ProcessSpec`] for
//! backends, a [`LaunchSpec`] for the gateway), so service mode can
//! never drift from process mode's flags, env, or ports. Updating a
//! definition is a file rewrite — no elevation, no SCM call; the shim
//! reads it at service start, so a running service keeps the
//! definition it started with and a stopped one takes the new
//! definition at its next start (the Activation Plan's semantics).
//!
//! This crate is the platform-neutral core: definition types, the
//! pure builders, definition file I/O, env resolution, and the
//! [`ServiceControl`] trait. The real SCM implementation is
//! `cfg(windows)` in [`scm`]; other platforms get a stub whose every
//! operation reports [`ServiceError::Unsupported`], so the workspace
//! builds and the definition tests run on Linux CI.

pub mod scm;

use manager_supervisor::lifecycle::LifecycleState;
use manager_supervisor::{ProcessSpec, ProcessState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Definition format version written by [`write_definition`].
pub const DEFINITION_VERSION: u32 = 1;

/// Service name of the gateway service.
pub const GATEWAY_SERVICE_NAME: &str = "llm-gateway";

/// Service name for a backend id (`llm-backend-<id>`).
pub fn backend_service_name(backend_id: &str) -> String {
    format!("llm-backend-{backend_id}")
}

/// Directory holding the service definition files (`<data_dir>/services`).
pub fn services_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("services")
}

/// Definition file path for a service name.
pub fn definition_path(data_dir: &Path, service_name: &str) -> PathBuf {
    services_dir(data_dir).join(format!("{service_name}.json"))
}

/// One environment value in a definition. Secrets are never stored as
/// literals: the shim resolves references at service start, in the
/// service account's context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvValue {
    /// A plain value, stored as-is.
    Literal { value: String },
    /// The value of a Windows Credential Manager generic credential
    /// (its target name), read by the shim at start. A missing
    /// credential fails the service start: a gateway without its
    /// master key must not come up half-configured.
    CredentialRef { target: String },
    /// The gateway's `DATABASE_URL`, composed by the shim at start
    /// from the Postgres password credential:
    /// `postgresql://postgres:<password>@127.0.0.1:<port>/litellm`
    /// (mirrors `postgres_database_url` in manager-wizard). A missing
    /// password credential means the DatabaseUrl entry is omitted —
    /// the no-database gateway, same as a process-mode start without
    /// Postgres installed.
    DatabaseUrl {
        password_credential: String,
        port: u16,
    },
}

/// One named environment entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvEntry {
    pub name: String,
    pub value: EnvValue,
}

impl EnvEntry {
    /// A literal value, written into the definition file as-is — never for secrets.
    pub fn literal(name: &str, value: String) -> Self {
        Self {
            name: name.to_string(),
            value: EnvValue::Literal { value },
        }
    }

    /// A value the shim reads from Credential Manager target `target` at service start; a
    /// missing credential fails the start.
    pub fn credential_ref(name: &str, target: &str) -> Self {
        Self {
            name: name.to_string(),
            value: EnvValue::CredentialRef {
                target: target.to_string(),
            },
        }
    }
}

/// When the SCM starts the service. Manual is the default for every
/// managed service: the app owns the running set (an `enabled = false`
/// backend must never auto-start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServiceStartMode {
    #[default]
    Manual,
    Automatic,
}

/// SCM recovery actions for a managed service (restart with growing
/// delays; the failure counter resets after `reset_period_secs`).
///
/// The SCM runs these actions only when the service *process* (the
/// shim) dies, unless the registration also sets
/// `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`. [`Self::restart_on_child_exit`]
/// controls that flag: with it, an unexpected child exit (which the
/// shim reports as `ERROR_SERVICE_SPECIFIC_ERROR` + the child's exit
/// code) also triggers the restart actions. An operator stop is always
/// reported as a clean `SERVICE_STOPPED` (exit 0), so it never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureActions {
    pub first_delay_secs: u32,
    pub second_delay_secs: u32,
    pub subsequent_delay_secs: u32,
    pub reset_period_secs: u32,
    /// Run the restart actions when the shim's child exits
    /// unexpectedly (`fFailureActionsOnNonCrashFailures`). True only
    /// for the log receiver: the app owns the gateway and backends,
    /// and neither mode auto-restarts them (one crash policy for
    /// process mode and service mode).
    #[serde(default)]
    pub restart_on_child_exit: bool,
}

impl Default for FailureActions {
    fn default() -> Self {
        Self {
            first_delay_secs: 10,
            second_delay_secs: 30,
            subsequent_delay_secs: 60,
            reset_period_secs: 3600,
            restart_on_child_exit: false,
        }
    }
}

/// A complete service definition — everything the shim needs to run
/// one service, and everything registration needs beyond the account
/// password (which is never persisted anywhere by this app; the SCM
/// stores it as an LSA secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDefinition {
    /// [`DEFINITION_VERSION`] at write time.
    pub version: u32,
    /// SCM service name (`llm-backend-<id>` or `llm-gateway`).
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// The real program the shim spawns (llama-server, python.exe
    /// for the gateway — pythonw would leave the gateway's console
    /// children, notably the Prisma query engine, with no hidden
    /// console to inherit).
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvEntry>,
    pub workdir: Option<PathBuf>,
    pub stdout_log: Option<PathBuf>,
    pub stderr_log: Option<PathBuf>,
    /// When set, the shim writes the child's pid here at start and
    /// removes it at stop. The gateway uses the app's existing
    /// `gateway.pid` path so pid-based machinery keeps working.
    pub pid_file: Option<PathBuf>,
    /// Account the service runs as (`DOMAIN\user`). `None` means the
    /// registration default (the interactive user, resolved by the
    /// elevated helper). Backends need no secrets and could run as a
    /// system account later; increment 1 uses one account for all
    /// managed services (design note §4b).
    pub account: Option<String>,
    pub start_mode: ServiceStartMode,
    pub failure_actions: FailureActions,
    /// The service's lifecycle marker log (`<scope>.lifecycle.log`),
    /// where the shim records service start, child spawn, stop
    /// requests and child exits in the shared
    /// `manager_supervisor::lifecycle::marker_line` format. Definitions
    /// written before this field existed derive it from `stdout_log`
    /// (see [`lifecycle_log_path`]).
    #[serde(default)]
    pub lifecycle_log: Option<PathBuf>,
}

impl ServiceDefinition {
    /// The ImagePath registered with the SCM: the shim plus this
    /// definition's path, quoted for the command line.
    pub fn image_path(&self, host_exe: &Path, definition_file: &Path) -> String {
        format!(
            "{} run --definition {}",
            quote_arg(&host_exe.to_string_lossy()),
            quote_arg(&definition_file.to_string_lossy())
        )
    }
}

/// A platform-neutral launch description — the gateway's input to
/// [`gateway_definition`]. The start-gateway step builds it from its
/// `PlannedCommand` (transforming secret env values into references);
/// Settings builds it from config for registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvEntry>,
    pub workdir: Option<PathBuf>,
}

/// Build the definition for a backend from the supervisor's spawn
/// spec — a 1:1 conversion, so the service launches llama-server
/// exactly as process supervision does (same flags, tensor split,
/// `CUDA_VISIBLE_DEVICES`). Stdio goes to per-backend log files under
/// the app's log dir (the supervisor's in-memory rings cannot capture
/// a process the app did not spawn).
pub fn backend_definition(spec: &ProcessSpec, log_dir: &Path) -> ServiceDefinition {
    let port_note = spec
        .port
        .map(|p| format!(", port {p}"))
        .unwrap_or_default();
    ServiceDefinition {
        version: DEFINITION_VERSION,
        name: backend_service_name(&spec.id),
        display_name: format!("LLM Backend {}", spec.id),
        description: format!(
            "llama-server backend '{}' managed by Local LLM Service Manager{port_note}",
            spec.id
        ),
        program: spec.program.clone(),
        args: spec.args.clone(),
        env: spec
            .env
            .iter()
            .map(|(k, v)| EnvEntry::literal(k, v.clone()))
            .collect(),
        workdir: spec.workdir.clone(),
        stdout_log: Some(log_dir.join(format!("backend-{}.stdout.log", spec.id))),
        stderr_log: Some(log_dir.join(format!("backend-{}.stderr.log", spec.id))),
        // The child (llama-server) pid, so service-mode backends report
        // the real model process (VRAM attribution, uptime) instead of
        // the shim.
        pid_file: Some(log_dir.join(format!("backend-{}.pid", spec.id))),
        account: None,
        start_mode: ServiceStartMode::Manual,
        failure_actions: FailureActions::default(),
        lifecycle_log: Some(log_dir.join(format!("backend-{}.lifecycle.log", spec.id))),
    }
}

/// Build the definition for the gateway service from a launch spec.
/// The gateway's logs are the same files process mode writes, and
/// its pid file is the app's existing `gateway.pid`, so log tails,
/// diagnostics, and pid-based stop logic keep working.
pub fn gateway_definition(
    launch: LaunchSpec,
    gateway_stdout_log: PathBuf,
    gateway_stderr_log: PathBuf,
    gateway_pid_file: PathBuf,
) -> ServiceDefinition {
    let lifecycle_log = sibling_lifecycle_log(&gateway_stdout_log, "gateway");
    ServiceDefinition {
        version: DEFINITION_VERSION,
        name: GATEWAY_SERVICE_NAME.to_string(),
        display_name: "LLM Gateway (LiteLLM)".to_string(),
        description: "LiteLLM gateway managed by Local LLM Service Manager".to_string(),
        program: launch.program,
        args: launch.args,
        env: launch.env,
        workdir: launch.workdir,
        stdout_log: Some(gateway_stdout_log),
        stderr_log: Some(gateway_stderr_log),
        pid_file: Some(gateway_pid_file),
        account: None,
        start_mode: ServiceStartMode::Manual,
        failure_actions: FailureActions::default(),
        lifecycle_log: Some(lifecycle_log),
    }
}

/// `<dir of log>/<stem>.lifecycle.log`.
fn sibling_lifecycle_log(log: &Path, stem: &str) -> PathBuf {
    log.parent()
        .unwrap_or(Path::new("."))
        .join(format!("{stem}.lifecycle.log"))
}

/// The lifecycle marker log of a definition: its `lifecycle_log`, or,
/// for a definition written before that field existed, the
/// `<stem>.lifecycle.log` sibling of its stdout log (`backend-x.stdout.log`
/// → `backend-x.lifecycle.log`). `None` when the definition has no
/// log files at all.
pub fn lifecycle_log_path(def: &ServiceDefinition) -> Option<PathBuf> {
    if let Some(p) = &def.lifecycle_log {
        return Some(p.clone());
    }
    let log = def.stdout_log.as_ref().or(def.stderr_log.as_ref())?;
    let file = log.file_name()?.to_string_lossy().into_owned();
    let stem = file
        .strip_suffix(".stdout.log")
        .or_else(|| file.strip_suffix(".stderr.log"))
        .or_else(|| file.strip_suffix(".log"))
        .unwrap_or(&file)
        .to_string();
    Some(sibling_lifecycle_log(log, &stem))
}

/// The app-side lifecycle key for an SCM service name, as used in
/// lifecycle markers and events: `llm-gateway` → `gateway`,
/// `llm-log-receiver` → `log-receiver`, `llm-backend-<id>` →
/// `backend:<id>`; any other name is returned unchanged.
pub fn lifecycle_key(service_name: &str) -> String {
    if service_name == GATEWAY_SERVICE_NAME {
        "gateway".to_string()
    } else if service_name == LOG_RECEIVER_SERVICE_NAME {
        "log-receiver".to_string()
    } else if let Some(id) = service_name.strip_prefix("llm-backend-") {
        format!("backend:{id}")
    } else {
        service_name.to_string()
    }
}

/// Service name of the Prometheus-local log receiver
/// (`crates/manager-logreceiver`; design:
/// DESIGN-prometheus-local.md, Oct 2026).
pub const LOG_RECEIVER_SERVICE_NAME: &str = "llm-log-receiver";

/// The log receiver's directory under the data dir: its SQLite
/// store, its token files, and its service log files live here.
pub fn log_receiver_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("log-receiver")
}

/// Build the definition for the log receiver service.
///
/// Unlike backends and the gateway, the receiver is **automatic**
/// start and runs as `NT AUTHORITY\LocalService`: it must be up
/// before and independently of the app (Muse reads stored logs
/// while LLMdil is down, including logs from the run that crashed),
/// and it needs write access to its own directory and nothing else.
/// All of its configuration travels as literal env entries pointing
/// at token *files* in [`log_receiver_dir`] — token values are never
/// in the definition (the shipper's ingest token keeps its existing
/// Credential Manager home on the app side; the service account
/// cannot read the operator's credential store, so the receiver's
/// copies are ACL'd files, seeded once by the deploy script).
/// Failure actions are the design's: restart 5 s / 30 s / 5 min,
/// counter reset after 24 h — SCM recovery replaces the retired
/// VM-side watchdog's repair role. `restart_on_child_exit` makes those
/// actions fire when the receiver process exits unexpectedly (the shim
/// stays alive and reports a service-specific failure), not only when
/// the shim itself dies.
pub fn log_receiver_definition(data_dir: &Path, receiver_exe: &Path) -> ServiceDefinition {
    let dir = log_receiver_dir(data_dir);
    let log_dir = dir.join("logs");
    ServiceDefinition {
        version: DEFINITION_VERSION,
        name: LOG_RECEIVER_SERVICE_NAME.to_string(),
        display_name: "LLMdil Log Receiver".to_string(),
        description: "Self-hosted ingest + store for LLMdil log streaming. \
                      Ingest on 127.0.0.1:8473; reads on the machine's tailnet \
                      address only (port 8474, token-gated)."
            .to_string(),
        program: receiver_exe.to_path_buf(),
        args: Vec::new(),
        env: vec![
            EnvEntry::literal(
                "LLMDIL_LOG_RECEIVER_DB",
                dir.join("llmdil-logs.db").to_string_lossy().into_owned(),
            ),
            EnvEntry::literal(
                "LLMDIL_LOG_INGEST_TOKEN_FILE",
                dir.join("ingest-tokens.txt").to_string_lossy().into_owned(),
            ),
            EnvEntry::literal(
                "LLMDIL_LOG_READ_TOKEN_FILE",
                dir.join("read-token.txt").to_string_lossy().into_owned(),
            ),
            EnvEntry::literal("LLMDIL_LOG_READ_PORT", "8474".to_string()),
            EnvEntry::literal("LLMDIL_LOG_RETENTION_DAYS", "14".to_string()),
            EnvEntry::literal("LLMDIL_LOG_RETENTION_CAP_MIB", "512".to_string()),
        ],
        workdir: Some(dir.clone()),
        stdout_log: Some(log_dir.join("receiver.stdout.log")),
        stderr_log: Some(log_dir.join("receiver.stderr.log")),
        pid_file: None,
        account: Some("NT AUTHORITY\\LocalService".to_string()),
        start_mode: ServiceStartMode::Automatic,
        failure_actions: FailureActions {
            first_delay_secs: 5,
            second_delay_secs: 30,
            subsequent_delay_secs: 300,
            reset_period_secs: 86_400,
            restart_on_child_exit: true,
        },
        lifecycle_log: Some(log_dir.join("receiver.lifecycle.log")),
    }
}

/// Quote one argument for a Windows command line (the
/// `CommandLineToArgvW` rules: quote when the value contains spaces,
/// quotes, or a trailing backslash; double embedded quotes and the
/// backslashes that precede them).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg.contains([' ', '\t', '"'])
        && !arg.ends_with('\\')
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Write a definition file atomically (temp file + rename), creating
/// the services directory. Returns the definition's path.
pub fn write_definition(data_dir: &Path, def: &ServiceDefinition) -> Result<PathBuf, ServiceError> {
    let dir = services_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| ServiceError::Io {
        path: dir.clone(),
        reason: e.to_string(),
    })?;
    let path = definition_path(data_dir, &def.name);
    let json = serde_json::to_string_pretty(def).map_err(|e| ServiceError::Definition {
        reason: format!("serialize definition for {}: {e}", def.name),
    })?;
    let tmp = dir.join(format!(".{}.json.tmp", def.name));
    std::fs::write(&tmp, json).map_err(|e| ServiceError::Io {
        path: tmp.clone(),
        reason: e.to_string(),
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| ServiceError::Io {
        path: path.clone(),
        reason: e.to_string(),
    })?;
    Ok(path)
}

/// Read a definition file the shim (or a sync path) previously wrote.
pub fn read_definition(path: &Path) -> Result<ServiceDefinition, ServiceError> {
    let text = std::fs::read_to_string(path).map_err(|e| ServiceError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    serde_json::from_str(&text).map_err(|e| ServiceError::Definition {
        reason: format!("parse {}: {e}", path.display()),
    })
}

/// Resolve a definition's env entries into concrete pairs, reading
/// credential references through `read_cred`. This is the shim's
/// start path; the resolver is injected so the rules are testable
/// without Credential Manager. See [`EnvValue`] for the per-kind
/// missing-credential behavior.
pub fn resolve_env(
    def: &ServiceDefinition,
    read_cred: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, ServiceError> {
    let mut out = Vec::with_capacity(def.env.len());
    for entry in &def.env {
        match &entry.value {
            EnvValue::Literal { value } => out.push((entry.name.clone(), value.clone())),
            EnvValue::CredentialRef { target } => {
                let value = read_cred(target).ok_or_else(|| {
                    ServiceError::CredentialMissing {
                        name: entry.name.clone(),
                        target: target.clone(),
                    }
                })?;
                out.push((entry.name.clone(), value));
            }
            EnvValue::DatabaseUrl {
                password_credential,
                port,
            } => {
                if let Some(password) = read_cred(password_credential) {
                    out.push((
                        entry.name.clone(),
                        format!("postgresql://postgres:{password}@127.0.0.1:{port}/litellm"),
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// SCM-reported lifecycle state of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    Paused,
    /// A raw SCM state this crate does not model.
    Unknown(u32),
}

/// Point-in-time status of one service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
    /// The service process (shim) pid when running.
    pub pid: Option<u32>,
    /// The shim's child pid, when the definition carries a pid file
    /// and the shim has written it (the gateway). Filled in by
    /// `ServiceControl::query` callers that know the definition; the SCM itself
    /// only reports the service's own pid.
    pub child_pid: Option<u32>,
    pub win32_exit_code: u32,
    /// `dwServiceSpecificExitCode`, meaningful when `win32_exit_code`
    /// is [`ERROR_SERVICE_SPECIFIC_ERROR`]. The shim reports 1 for an
    /// env/credential resolution failure, 2 for a spawn failure, and
    /// the child's own exit code for an unexpected child exit.
    #[serde(default)]
    pub service_specific_exit_code: u32,
}

impl ServiceState {
    /// Wire/display spelling (matches the serde form; `Unknown(_)` is
    /// `"unknown"`).
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::NotInstalled => "not_installed",
            ServiceState::Stopped => "stopped",
            ServiceState::StartPending => "start_pending",
            ServiceState::StopPending => "stop_pending",
            ServiceState::Running => "running",
            ServiceState::Paused => "paused",
            ServiceState::Unknown(_) => "unknown",
        }
    }
}

/// `ERROR_SERVICE_SPECIFIC_ERROR`: the shim's "see the service-specific
/// exit code" status after a failed start or an unexpected child exit.
pub const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;
/// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL`: a control sent while the service
/// is start- or stop-pending (or, for older shims, start-pending only).
pub const ERROR_SERVICE_CANNOT_ACCEPT_CTRL: u32 = 1061;
/// `ERROR_SERVICE_MARKED_FOR_DELETE`: a create/open against a service
/// whose deletion is pending until its process exits.
pub const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// The shim's service-specific exit code for an env/credential
/// resolution failure.
pub const HOST_EXIT_ENV_FAILURE: u32 = 1;
/// The shim's service-specific exit code for a child spawn failure.
pub const HOST_EXIT_SPAWN_FAILURE: u32 = 2;

/// A child exit code as text: decimal, plus hex for NTSTATUS-style
/// values (`3221225477 (0xC0000005)`).
fn exit_code_text(code: u32) -> String {
    if code > 0xFFFF {
        format!("{code} (0x{code:08X})")
    } else {
        code.to_string()
    }
}

impl ServiceStatus {
    /// True when the service is stopped with a non-zero exit status (a
    /// failed start or an unexpected child exit), as opposed to a clean
    /// operator stop.
    pub fn stopped_with_error(&self) -> bool {
        matches!(self.state, ServiceState::Stopped)
            && (self.win32_exit_code != 0 || self.service_specific_exit_code != 0)
    }

    /// The shared lifecycle state for this SCM status (design state
    /// model §1): Stopped with any non-zero exit code is `failed`,
    /// Paused (the process is there but not serving) is `degraded`.
    pub fn lifecycle_state(&self) -> LifecycleState {
        match self.state {
            ServiceState::Running => LifecycleState::Running,
            ServiceState::StartPending => LifecycleState::Starting,
            ServiceState::StopPending => LifecycleState::Stopping,
            ServiceState::Stopped if self.stopped_with_error() => LifecycleState::Failed,
            ServiceState::Stopped => LifecycleState::Stopped,
            ServiceState::Paused => LifecycleState::Degraded,
            ServiceState::Unknown(_) => LifecycleState::Unknown,
            ServiceState::NotInstalled => LifecycleState::NotInstalled,
        }
    }

    /// Why the service is in `failed`, from its exit codes; `None` for
    /// any other state.
    pub fn failure_reason(&self) -> Option<String> {
        if !self.stopped_with_error() {
            return None;
        }
        if self.win32_exit_code == ERROR_SERVICE_SPECIFIC_ERROR {
            return Some(match self.service_specific_exit_code {
                HOST_EXIT_ENV_FAILURE => {
                    "service host could not resolve the environment/credentials".to_string()
                }
                HOST_EXIT_SPAWN_FAILURE => {
                    "service host could not spawn the program".to_string()
                }
                code => format!("child exited with code {}", exit_code_text(code)),
            });
        }
        // Older shims reported the child's exit code as the Win32 code.
        Some(format!(
            "child exited with code {}",
            exit_code_text(self.win32_exit_code)
        ))
    }

    /// Map onto the supervisor's backend state model. `NotInstalled`
    /// maps to `Stopped` — callers distinguish it via
    /// [`ServiceState`] before mapping. Stopped with a non-zero exit
    /// code (a crash or a failed start) maps to `Failed`.
    pub fn process_state(&self) -> ProcessState {
        match self.state {
            ServiceState::Running => ProcessState::Running,
            ServiceState::StartPending => ProcessState::Starting,
            ServiceState::StopPending => ProcessState::Stopping,
            ServiceState::Paused => ProcessState::Failed,
            ServiceState::Stopped if self.stopped_with_error() => ProcessState::Failed,
            ServiceState::Stopped
            | ServiceState::NotInstalled
            | ServiceState::Unknown(_) => ProcessState::Stopped,
        }
    }
}

/// Operations the app performs against the SCM. Implemented for real
/// on Windows (`scm::WindowsServiceControl`); elsewhere
/// ([`scm::UnsupportedServiceControl`]) every operation fails with
/// [`ServiceError::Unsupported`].
pub trait ServiceControl: Send + Sync {
    /// Status of one service; `NotInstalled` is a state, not an error.
    fn query(&self, name: &str) -> Result<ServiceStatus, ServiceError>;
    /// Whether the service exists in the SCM; derived from [`Self::query`].
    fn is_registered(&self, name: &str) -> Result<bool, ServiceError> {
        Ok(!matches!(self.query(name)?.state, ServiceState::NotInstalled))
    }
    /// Ask the SCM to start the service. An already-running service counts as success.
    /// Returns once the SCM accepts the request, not when the service reaches `Running`.
    fn start(&self, name: &str) -> Result<(), ServiceError>;
    /// Request a stop. Returns once the SCM has accepted the control;
    /// the service may still be `StopPending` — callers that need the
    /// terminal state poll [`ServiceControl::query`].
    fn stop(&self, name: &str) -> Result<(), ServiceError>;
    /// Create the service (or fail if it already exists). Needs an
    /// administrator token; the app runs it through the elevated
    /// helper. `password` is the service account's password and is
    /// passed only to the SCM (stored by Windows as an LSA secret).
    fn register(
        &self,
        def: &ServiceDefinition,
        image_path: &str,
        password: Option<&str>,
    ) -> Result<(), ServiceError>;
    /// Delete the service. Needs an administrator token. A running
    /// service is marked for deletion and disappears when it stops.
    fn unregister(&self, name: &str) -> Result<(), ServiceError>;
}

/// Errors from the service layer.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("io error on {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("invalid service definition: {reason}")]
    Definition { reason: String },
    #[error("service '{0}' is not registered")]
    NotRegistered(String),
    #[error("service control operation '{operation}' failed with Windows error {code}")]
    Scm { operation: &'static str, code: u32 },
    #[error("windows services are not supported on this platform: {0}")]
    Unsupported(String),
    #[error("credential '{target}' for environment variable {name} was not found")]
    CredentialMissing { name: String, target: String },
    /// A wait helper ran out of time.
    #[error("service '{name}' did not reach {waiting_for} within {} s (last state: {last_state})", waited_ms.div_ceil(1000))]
    Timeout {
        name: String,
        waiting_for: String,
        last_state: &'static str,
        waited_ms: u64,
    },
    /// [`start_and_wait`]: the service fell back to Stopped instead of
    /// reaching Running.
    #[error("service '{name}' failed to start: {reason}")]
    StartFailed {
        name: String,
        reason: String,
        status: Box<ServiceStatus>,
    },
}

impl ServiceError {
    /// The Win32 error code of an SCM call failure.
    pub fn win32_code(&self) -> Option<u32> {
        match self {
            ServiceError::Scm { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// True for `ERROR_SERVICE_CANNOT_ACCEPT_CTRL` (the service is
    /// start- or stop-pending and cannot take the control now).
    pub fn is_cannot_accept_control(&self) -> bool {
        self.win32_code() == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL)
    }
}

/// Result of [`stop_and_wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StopOutcome {
    /// No such service: nothing to stop.
    NotRegistered,
    /// The service was already stopped when asked.
    AlreadyStopped,
    /// The service was running (or pending) and is now stopped (or,
    /// when its deletion was pending, gone).
    Stopped { waited_ms: u64 },
}

/// The one table of SCM wait budgets (replaces the per-call-site
/// 15 s / 30 s loops).
pub struct ServiceTimeouts;

impl ServiceTimeouts {
    /// Poll interval of every wait helper.
    pub const POLL: Duration = Duration::from_millis(100);

    /// Budget for a stop to reach Stopped. The shim terminates its job
    /// object on the stop control, so this is generous for every
    /// service; the same for all (the gateway used to get 15 s and
    /// backends 30 s for the same operation).
    pub fn stop(_name: &str) -> Duration {
        Duration::from_secs(30)
    }

    /// Budget for a start to reach Running. The shim reports Running
    /// once the child is spawned (the gateway's pid file is written
    /// before that), so 30 s covers credential resolution and process
    /// creation on a loaded machine.
    pub fn start(_name: &str) -> Duration {
        Duration::from_secs(30)
    }

    /// Budget for a deleted registration to disappear (it lingers while
    /// the old service process is still exiting).
    pub fn unregister(_name: &str) -> Duration {
        Duration::from_secs(15)
    }
}

fn timeout_error(name: &str, waiting_for: &str, last: ServiceState, started: Instant) -> ServiceError {
    ServiceError::Timeout {
        name: name.to_string(),
        waiting_for: waiting_for.to_string(),
        last_state: last.as_str(),
        waited_ms: started.elapsed().as_millis() as u64,
    }
}

/// Calls `on_change` the first time and whenever the state changes.
struct ChangeTracker<'a> {
    last: Option<ServiceState>,
    on_change: &'a mut dyn FnMut(&ServiceStatus),
}

impl ChangeTracker<'_> {
    fn see(&mut self, status: &ServiceStatus) {
        if self.last != Some(status.state) {
            self.last = Some(status.state);
            (self.on_change)(status);
        }
    }
}

/// Poll `name` every [`ServiceTimeouts::POLL`] until its state is one of
/// `accept`; returns that status. `on_change` sees the first status and
/// every state change (progress for markers/UI). Query errors end the
/// wait; running out of time is [`ServiceError::Timeout`].
pub fn wait_for_state(
    control: &dyn ServiceControl,
    name: &str,
    accept: &[ServiceState],
    timeout: Duration,
    on_change: &mut dyn FnMut(&ServiceStatus),
) -> Result<ServiceStatus, ServiceError> {
    wait_for_state_polling(control, name, accept, timeout, ServiceTimeouts::POLL, on_change)
}

fn state_matches(state: ServiceState, accept: &[ServiceState]) -> bool {
    accept.iter().any(|a| match (a, state) {
        // Any raw state matches an `Unknown` entry.
        (ServiceState::Unknown(_), ServiceState::Unknown(_)) => true,
        (a, s) => *a == s,
    })
}

fn wait_for_state_polling(
    control: &dyn ServiceControl,
    name: &str,
    accept: &[ServiceState],
    timeout: Duration,
    poll: Duration,
    on_change: &mut dyn FnMut(&ServiceStatus),
) -> Result<ServiceStatus, ServiceError> {
    let started = Instant::now();
    let mut tracker = ChangeTracker { last: None, on_change };
    loop {
        let status = control.query(name)?;
        tracker.see(&status);
        if state_matches(status.state, accept) {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let wanted = accept
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("/");
            return Err(timeout_error(name, &wanted, status.state, started));
        }
        std::thread::sleep(poll);
    }
}

/// Stop `name` and wait until it is Stopped (or gone). Blocking; run it
/// on a blocking thread from async code.
///
/// - Not registered → [`StopOutcome::NotRegistered`]; already Stopped →
///   [`StopOutcome::AlreadyStopped`].
/// - StopPending: someone else's stop is in flight; just wait.
/// - StartPending: the stop control is sent (current shims accept it
///   while starting); a shim that refuses it with
///   `ERROR_SERVICE_CANNOT_ACCEPT_CTRL` (1061) is waited on until it is
///   Running and then stopped.
/// - A 1061 from a race (the service turned pending between the query
///   and the control) is "keep waiting", never an error.
pub fn stop_and_wait(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
) -> Result<StopOutcome, ServiceError> {
    stop_and_wait_with(control, name, timeout, &mut |_| {})
}

/// [`stop_and_wait`] with a progress callback (first status and every
/// state change).
pub fn stop_and_wait_with(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
    on_change: &mut dyn FnMut(&ServiceStatus),
) -> Result<StopOutcome, ServiceError> {
    stop_and_wait_polling(control, name, timeout, ServiceTimeouts::POLL, on_change)
}

fn stop_and_wait_polling(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
    poll: Duration,
    on_change: &mut dyn FnMut(&ServiceStatus),
) -> Result<StopOutcome, ServiceError> {
    let started = Instant::now();
    let mut tracker = ChangeTracker { last: None, on_change };
    let mut first = true;
    let mut stop_sent = false;
    loop {
        let status = control.query(name)?;
        tracker.see(&status);
        let waited_ms = started.elapsed().as_millis() as u64;
        match status.state {
            ServiceState::NotInstalled if first => return Ok(StopOutcome::NotRegistered),
            // Deleted while we waited (a pending deletion completes
            // when the process exits): stopped for every purpose.
            ServiceState::NotInstalled => return Ok(StopOutcome::Stopped { waited_ms }),
            ServiceState::Stopped if first => return Ok(StopOutcome::AlreadyStopped),
            ServiceState::Stopped => return Ok(StopOutcome::Stopped { waited_ms }),
            ServiceState::StopPending => {}
            ServiceState::StartPending
            | ServiceState::Running
            | ServiceState::Paused
            | ServiceState::Unknown(_) => {
                if !stop_sent {
                    match control.stop(name) {
                        Ok(()) => stop_sent = true,
                        // Pending: retry on the next poll.
                        Err(e) if e.is_cannot_accept_control() => {}
                        Err(ServiceError::NotRegistered(_)) => {
                            return Ok(if first {
                                StopOutcome::NotRegistered
                            } else {
                                StopOutcome::Stopped { waited_ms }
                            });
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        first = false;
        if started.elapsed() >= timeout {
            return Err(timeout_error(name, "stopped", status.state, started));
        }
        std::thread::sleep(poll);
    }
}

/// Start `name` and wait until it is Running. Blocking.
///
/// - Not registered → [`ServiceError::NotRegistered`].
/// - Already Running → Ok at once; StartPending → wait without a second
///   start; StopPending → wait for the stop to finish, then start.
/// - Falling back to Stopped after the start →
///   [`ServiceError::StartFailed`] carrying
///   [`ServiceStatus::failure_reason`] (env/credential failure, spawn
///   failure, or the child's exit code).
pub fn start_and_wait(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
) -> Result<ServiceStatus, ServiceError> {
    start_and_wait_with(control, name, timeout, &mut |_| {})
}

/// [`start_and_wait`] with a progress callback.
pub fn start_and_wait_with(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
    on_change: &mut dyn FnMut(&ServiceStatus),
) -> Result<ServiceStatus, ServiceError> {
    start_and_wait_polling(control, name, timeout, ServiceTimeouts::POLL, on_change)
}

fn start_and_wait_polling(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
    poll: Duration,
    on_change: &mut dyn FnMut(&ServiceStatus),
) -> Result<ServiceStatus, ServiceError> {
    let started = Instant::now();
    let mut tracker = ChangeTracker { last: None, on_change };
    let mut start_sent = false;
    loop {
        let status = control.query(name)?;
        tracker.see(&status);
        match status.state {
            ServiceState::NotInstalled => {
                return Err(ServiceError::NotRegistered(name.to_string()))
            }
            ServiceState::Running => return Ok(status),
            ServiceState::Stopped if start_sent => {
                return Err(ServiceError::StartFailed {
                    name: name.to_string(),
                    reason: status
                        .failure_reason()
                        .unwrap_or_else(|| "the service stopped during start".to_string()),
                    status: Box::new(status),
                });
            }
            ServiceState::Stopped => {
                control.start(name)?;
                start_sent = true;
            }
            // Someone else's start (wait for it) or stop (wait for it to
            // finish, then start).
            ServiceState::StartPending => start_sent = true,
            ServiceState::StopPending | ServiceState::Paused | ServiceState::Unknown(_) => {}
        }
        if started.elapsed() >= timeout {
            return Err(timeout_error(name, "running", status.state, started));
        }
        std::thread::sleep(poll);
    }
}

/// Wait until `name` is no longer registered (a deleted service whose
/// process is still exiting lingers as "marked for delete"). Blocking.
pub fn wait_until_absent(
    control: &dyn ServiceControl,
    name: &str,
    timeout: Duration,
) -> Result<(), ServiceError> {
    wait_for_state(control, name, &[ServiceState::NotInstalled], timeout, &mut |_| {}).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> ProcessSpec {
        ProcessSpec {
            id: "planner".to_string(),
            program: PathBuf::from(r"C:\data\llamacpp\llama-server.exe"),
            args: vec![
                "-m".to_string(),
                r"C:\data\models\planner.gguf".to_string(),
                "--port".to_string(),
                "8081".to_string(),
                "--tensor-split".to_string(),
                "1".to_string(),
            ],
            env: vec![(
                "CUDA_VISIBLE_DEVICES".to_string(),
                "GPU-e9747c8d-45a6-0ad2-c808-3d6967ad4676".to_string(),
            )],
            workdir: None,
            port: Some(8081),
            low_priority: false,
        }
    }

    #[test]
    fn backend_definition_mirrors_the_spec() {
        let def = backend_definition(&sample_spec(), Path::new(r"C:\data\logs"));
        assert_eq!(def.name, "llm-backend-planner");
        assert_eq!(def.program, PathBuf::from(r"C:\data\llamacpp\llama-server.exe"));
        assert_eq!(def.args.len(), 6);
        assert_eq!(
            def.env,
            vec![EnvEntry::literal(
                "CUDA_VISIBLE_DEVICES",
                "GPU-e9747c8d-45a6-0ad2-c808-3d6967ad4676".to_string()
            )]
        );
        assert_eq!(def.start_mode, ServiceStartMode::Manual);
        assert_eq!(
            def.stdout_log,
            Some(Path::new(r"C:\data\logs").join("backend-planner.stdout.log"))
        );
        assert_eq!(def.failure_actions.first_delay_secs, 10);
    }

    #[test]
    fn backend_service_names_are_stable() {
        assert_eq!(backend_service_name("coder-fast"), "llm-backend-coder-fast");
        assert_eq!(GATEWAY_SERVICE_NAME, "llm-gateway");
    }

    #[test]
    fn gateway_definition_keeps_secret_env_as_references() {
        let launch = LaunchSpec {
            program: PathBuf::from(r"C:\data\venv\Scripts\pythonw.exe"),
            args: vec!["-c".to_string(), "bootstrap".to_string()],
            env: vec![
                EnvEntry::literal("PYTHONUTF8", "1".to_string()),
                EnvEntry::credential_ref(
                    "LITELLM_MASTER_KEY",
                    "local-llm-service-manager/litellm-master-key",
                ),
                EnvEntry {
                    name: "DATABASE_URL".to_string(),
                    value: EnvValue::DatabaseUrl {
                        password_credential:
                            "local-llm-service-manager/postgres-password".to_string(),
                        port: 54329,
                    },
                },
            ],
            workdir: None,
        };
        let def = gateway_definition(
            launch,
            PathBuf::from(r"C:\data\logs\gateway.stdout.log"),
            PathBuf::from(r"C:\data\logs\gateway.stderr.log"),
            PathBuf::from(r"C:\data\gateway.pid"),
        );
        assert_eq!(def.name, GATEWAY_SERVICE_NAME);
        // The serialized definition must not contain any secret value —
        // only credential target names.
        let json = serde_json::to_string(&def).expect("serialize");
        assert!(json.contains("litellm-master-key"));
        assert!(!json.contains("sk-"));
        let back: ServiceDefinition = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, def);
    }

    #[test]
    fn resolve_env_reads_literals_and_references() {
        let def = gateway_definition(
            LaunchSpec {
                program: PathBuf::from("pythonw.exe"),
                args: Vec::new(),
                env: vec![
                    EnvEntry::literal("A", "1".to_string()),
                    EnvEntry::credential_ref("KEY", "target/key"),
                    EnvEntry {
                        name: "DATABASE_URL".to_string(),
                        value: EnvValue::DatabaseUrl {
                            password_credential: "target/pg".to_string(),
                            port: 54329,
                        },
                    },
                ],
                workdir: None,
            },
            PathBuf::from("out.log"),
            PathBuf::from("err.log"),
            PathBuf::from("gw.pid"),
        );
        let resolved = resolve_env(&def, |target| match target {
            "target/key" => Some("secret-value".to_string()),
            "target/pg" => Some("pgpass".to_string()),
            _ => None,
        })
        .expect("resolve");
        assert_eq!(
            resolved,
            vec![
                ("A".to_string(), "1".to_string()),
                ("KEY".to_string(), "secret-value".to_string()),
                (
                    "DATABASE_URL".to_string(),
                    "postgresql://postgres:pgpass@127.0.0.1:54329/litellm".to_string()
                ),
            ]
        );
    }

    #[test]
    fn resolve_env_missing_master_key_fails_but_missing_db_password_omits() {
        let mut def = gateway_definition(
            LaunchSpec {
                program: PathBuf::from("pythonw.exe"),
                args: Vec::new(),
                env: vec![EnvEntry {
                    name: "DATABASE_URL".to_string(),
                    value: EnvValue::DatabaseUrl {
                        password_credential: "target/pg".to_string(),
                        port: 54329,
                    },
                }],
                workdir: None,
            },
            PathBuf::from("out.log"),
            PathBuf::from("err.log"),
            PathBuf::from("gw.pid"),
        );
        // No Postgres password → the entry is omitted, not an error.
        let resolved = resolve_env(&def, |_| None).expect("no-db resolve");
        assert!(resolved.is_empty());
        // A missing hard credential reference fails the start.
        def.env.push(EnvEntry::credential_ref("LITELLM_MASTER_KEY", "target/key"));
        let err = resolve_env(&def, |_| None).expect_err("must fail");
        assert!(matches!(err, ServiceError::CredentialMissing { .. }));
    }

    #[test]
    fn quote_arg_follows_windows_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(r"C:\path\file.exe"), r"C:\path\file.exe");
        assert_eq!(quote_arg(r"C:\Program Files\app.exe"), r#""C:\Program Files\app.exe""#);
        assert_eq!(quote_arg(r"C:\trailing\"), r#""C:\trailing\\""#);
        assert_eq!(quote_arg("a\"b"), r#""a\"b""#);
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn image_path_points_the_shim_at_the_definition() {
        let def = backend_definition(&sample_spec(), Path::new(r"C:\data\logs"));
        let image = def.image_path(
            Path::new(r"C:\Program Files\LLM\llm-service-host.exe"),
            Path::new(r"C:\data\services\llm-backend-planner.json"),
        );
        assert_eq!(
            image,
            r#""C:\Program Files\LLM\llm-service-host.exe" run --definition C:\data\services\llm-backend-planner.json"#
        );
    }

    #[test]
    fn definition_files_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "manager-services-test-{}",
            std::process::id()
        ));
        let def = backend_definition(&sample_spec(), Path::new("logs"));
        let path = write_definition(&dir, &def).expect("write");
        assert_eq!(path, definition_path(&dir, "llm-backend-planner"));
        let back = read_definition(&path).expect("read");
        assert_eq!(back, def);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_receiver_definition_is_automatic_local_service() {
        let data_dir = Path::new(r"C:\Users\pete\AppData\Roaming\local-llm-service-manager");
        let exe = Path::new(r"C:\Users\pete\AppData\Local\LLMdil\llmdil-log-receiver.exe");
        let def = log_receiver_definition(data_dir, exe);
        assert_eq!(def.name, LOG_RECEIVER_SERVICE_NAME);
        assert_eq!(def.name, "llm-log-receiver");
        assert_eq!(def.program, exe.to_path_buf());
        assert_eq!(def.start_mode, ServiceStartMode::Automatic);
        assert_eq!(def.account.as_deref(), Some("NT AUTHORITY\\LocalService"));
        assert_eq!(def.failure_actions.first_delay_secs, 5);
        assert_eq!(def.failure_actions.second_delay_secs, 30);
        assert_eq!(def.failure_actions.subsequent_delay_secs, 300);
        assert_eq!(def.failure_actions.reset_period_secs, 86_400);
        let lr = data_dir.join("log-receiver");
        assert_eq!(def.workdir, Some(lr.clone()));
        assert_eq!(
            def.stdout_log,
            Some(lr.join("logs").join("receiver.stdout.log"))
        );
        // Env points at files under the receiver dir; no token value
        // (and no credential reference) ever appears in a definition.
        let env: Vec<(&str, &str)> = def
            .env
            .iter()
            .map(|e| match &e.value {
                EnvValue::Literal { value } => (e.name.as_str(), value.as_str()),
                other => panic!("receiver env must be literals, got {other:?}"),
            })
            .collect();
        let expected_db = lr.join("llmdil-logs.db").to_string_lossy().into_owned();
        assert!(env.contains(&("LLMDIL_LOG_RECEIVER_DB", expected_db.as_str())));
        assert!(env.contains(&("LLMDIL_LOG_READ_PORT", "8474")));
        assert!(env.contains(&("LLMDIL_LOG_RETENTION_CAP_MIB", "512")));
        let json = serde_json::to_string(&def).expect("serialize");
        assert!(!json.contains("Bearer"));
        let back: ServiceDefinition = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, def);
    }

    #[test]
    fn scm_states_map_onto_the_backend_model() {
        let status = |state| ServiceStatus {
            name: "llm-backend-planner".to_string(),
            state,
            pid: None,
            child_pid: None,
            win32_exit_code: 0,
            service_specific_exit_code: 0,
        };
        assert_eq!(status(ServiceState::Running).process_state(), ProcessState::Running);
        assert_eq!(status(ServiceState::StartPending).process_state(), ProcessState::Starting);
        assert_eq!(status(ServiceState::StopPending).process_state(), ProcessState::Stopping);
        assert_eq!(status(ServiceState::Stopped).process_state(), ProcessState::Stopped);
        assert_eq!(status(ServiceState::NotInstalled).process_state(), ProcessState::Stopped);
        // A crash (or a failed start) is Failed, not a clean stop.
        let mut crashed = status(ServiceState::Stopped);
        crashed.win32_exit_code = ERROR_SERVICE_SPECIFIC_ERROR;
        crashed.service_specific_exit_code = 3;
        assert_eq!(crashed.process_state(), ProcessState::Failed);
    }

    fn st(state: ServiceState, win32: u32, specific: u32) -> ServiceStatus {
        ServiceStatus {
            name: "svc".to_string(),
            state,
            pid: None,
            child_pid: None,
            win32_exit_code: win32,
            service_specific_exit_code: specific,
        }
    }

    #[test]
    fn lifecycle_state_mapping_includes_exit_codes() {
        use LifecycleState as L;
        assert_eq!(st(ServiceState::Running, 0, 0).lifecycle_state(), L::Running);
        assert_eq!(st(ServiceState::StartPending, 0, 0).lifecycle_state(), L::Starting);
        assert_eq!(st(ServiceState::StopPending, 0, 0).lifecycle_state(), L::Stopping);
        assert_eq!(st(ServiceState::Stopped, 0, 0).lifecycle_state(), L::Stopped);
        assert_eq!(st(ServiceState::Stopped, 0, 0).failure_reason(), None);
        assert_eq!(st(ServiceState::Paused, 0, 0).lifecycle_state(), L::Degraded);
        assert_eq!(st(ServiceState::Unknown(42), 0, 0).lifecycle_state(), L::Unknown);
        assert_eq!(st(ServiceState::NotInstalled, 0, 0).lifecycle_state(), L::NotInstalled);

        let env = st(ServiceState::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, 1);
        assert_eq!(env.lifecycle_state(), L::Failed);
        assert_eq!(
            env.failure_reason().as_deref(),
            Some("service host could not resolve the environment/credentials")
        );
        let spawn = st(ServiceState::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, 2);
        assert_eq!(
            spawn.failure_reason().as_deref(),
            Some("service host could not spawn the program")
        );
        let child = st(ServiceState::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, 7);
        assert_eq!(child.failure_reason().as_deref(), Some("child exited with code 7"));
        let av = st(ServiceState::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, 0xC000_0005);
        assert_eq!(
            av.failure_reason().as_deref(),
            Some("child exited with code 3221225477 (0xC0000005)")
        );
        // An older shim reported the child's code as the Win32 code.
        let legacy = st(ServiceState::Stopped, 1, 0);
        assert_eq!(legacy.lifecycle_state(), L::Failed);
        assert_eq!(legacy.failure_reason().as_deref(), Some("child exited with code 1"));
        // A running service never has a failure reason.
        assert_eq!(
            st(ServiceState::Running, ERROR_SERVICE_SPECIFIC_ERROR, 2).failure_reason(),
            None
        );
    }

    /// A scripted [`ServiceControl`]: `query` replays `states` (the last
    /// one repeats); `stop`/`start` return the next scripted result and
    /// are recorded.
    struct FakeControl {
        states: std::sync::Mutex<std::collections::VecDeque<ServiceStatus>>,
        stop_results: std::sync::Mutex<std::collections::VecDeque<Result<(), ServiceError>>>,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl FakeControl {
        fn new(states: Vec<ServiceStatus>) -> Self {
            Self {
                states: std::sync::Mutex::new(states.into()),
                stop_results: std::sync::Mutex::new(Default::default()),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn with_stop_results(self, results: Vec<Result<(), ServiceError>>) -> Self {
            *self.stop_results.lock().unwrap() = results.into();
            self
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ServiceControl for FakeControl {
        fn query(&self, _name: &str) -> Result<ServiceStatus, ServiceError> {
            let mut states = self.states.lock().unwrap();
            let s = if states.len() > 1 {
                states.pop_front().unwrap()
            } else {
                states.front().cloned().expect("scripted state")
            };
            Ok(s)
        }
        fn start(&self, _name: &str) -> Result<(), ServiceError> {
            self.calls.lock().unwrap().push("start");
            Ok(())
        }
        fn stop(&self, _name: &str) -> Result<(), ServiceError> {
            self.calls.lock().unwrap().push("stop");
            self.stop_results.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }
        fn register(&self, _: &ServiceDefinition, _: &str, _: Option<&str>) -> Result<(), ServiceError> {
            Ok(())
        }
        fn unregister(&self, _name: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    const FAST: Duration = Duration::from_millis(1);
    const LONG: Duration = Duration::from_secs(10);

    fn cannot_accept() -> ServiceError {
        ServiceError::Scm {
            operation: "ControlService",
            code: ERROR_SERVICE_CANNOT_ACCEPT_CTRL,
        }
    }

    #[test]
    fn stop_and_wait_outcomes() {
        let fake = FakeControl::new(vec![st(ServiceState::NotInstalled, 0, 0)]);
        assert_eq!(
            stop_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap(),
            StopOutcome::NotRegistered
        );
        assert!(fake.calls().is_empty());

        let fake = FakeControl::new(vec![st(ServiceState::Stopped, 0, 0)]);
        assert_eq!(
            stop_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap(),
            StopOutcome::AlreadyStopped
        );
        assert!(fake.calls().is_empty());

        let fake = FakeControl::new(vec![
            st(ServiceState::Running, 0, 0),
            st(ServiceState::StopPending, 0, 0),
            st(ServiceState::Stopped, 0, 0),
        ]);
        let mut seen = Vec::new();
        let out =
            stop_and_wait_polling(&fake, "svc", LONG, FAST, &mut |s| seen.push(s.state)).unwrap();
        assert!(matches!(out, StopOutcome::Stopped { .. }));
        assert_eq!(fake.calls(), vec!["stop"]);
        assert_eq!(
            seen,
            vec![ServiceState::Running, ServiceState::StopPending, ServiceState::Stopped]
        );
    }

    #[test]
    fn stop_and_wait_keeps_waiting_through_pending_and_1061() {
        // Already stop-pending (someone else's stop): no control sent,
        // just wait.
        let fake = FakeControl::new(vec![
            st(ServiceState::StopPending, 0, 0),
            st(ServiceState::StopPending, 0, 0),
            st(ServiceState::Stopped, 0, 0),
        ]);
        let out = stop_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap();
        assert!(matches!(out, StopOutcome::Stopped { .. }));
        assert!(fake.calls().is_empty());

        // Start-pending on an older shim: the stop is refused with 1061,
        // retried once the service is Running, then waited out.
        let fake = FakeControl::new(vec![
            st(ServiceState::StartPending, 0, 0),
            st(ServiceState::StartPending, 0, 0),
            st(ServiceState::Running, 0, 0),
            st(ServiceState::StopPending, 0, 0),
            st(ServiceState::Stopped, 0, 0),
        ])
        .with_stop_results(vec![Err(cannot_accept()), Err(cannot_accept()), Ok(())]);
        let out = stop_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap();
        assert!(matches!(out, StopOutcome::Stopped { .. }));
        assert_eq!(fake.calls(), vec!["stop", "stop", "stop"]);

        // Any other control error is an error.
        let fake = FakeControl::new(vec![st(ServiceState::Running, 0, 0)]).with_stop_results(vec![
            Err(ServiceError::Scm {
                operation: "ControlService",
                code: 5,
            }),
        ]);
        let err = stop_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap_err();
        assert_eq!(err.win32_code(), Some(5));

        // A stop that never completes times out.
        let fake = FakeControl::new(vec![st(ServiceState::StopPending, 0, 0)]);
        let err = stop_and_wait_polling(&fake, "svc", Duration::from_millis(20), FAST, &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, ServiceError::Timeout { last_state: "stop_pending", .. }), "{err}");
    }

    #[test]
    fn start_and_wait_reports_the_shim_failure_reason() {
        let fake = FakeControl::new(vec![
            st(ServiceState::Stopped, 0, 0),
            st(ServiceState::StartPending, 0, 0),
            st(ServiceState::Stopped, ERROR_SERVICE_SPECIFIC_ERROR, HOST_EXIT_SPAWN_FAILURE),
        ]);
        let err = start_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap_err();
        match &err {
            ServiceError::StartFailed { reason, status, .. } => {
                assert_eq!(reason, "service host could not spawn the program");
                assert_eq!(status.lifecycle_state(), LifecycleState::Failed);
            }
            other => panic!("expected StartFailed, got {other:?}"),
        }
        assert_eq!(fake.calls(), vec!["start"]);

        // Happy path.
        let fake = FakeControl::new(vec![
            st(ServiceState::Stopped, 0, 0),
            st(ServiceState::StartPending, 0, 0),
            st(ServiceState::Running, 0, 0),
        ]);
        let ok = start_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap();
        assert_eq!(ok.state, ServiceState::Running);

        // Stop-pending first: wait for the stop, then start.
        let fake = FakeControl::new(vec![
            st(ServiceState::StopPending, 0, 0),
            st(ServiceState::Stopped, 0, 0),
            st(ServiceState::Running, 0, 0),
        ]);
        start_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap();
        assert_eq!(fake.calls(), vec!["start"]);

        // Already running: no start call.
        let fake = FakeControl::new(vec![st(ServiceState::Running, 0, 0)]);
        start_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}).unwrap();
        assert!(fake.calls().is_empty());

        // Not registered.
        let fake = FakeControl::new(vec![st(ServiceState::NotInstalled, 0, 0)]);
        assert!(matches!(
            start_and_wait_polling(&fake, "svc", LONG, FAST, &mut |_| {}),
            Err(ServiceError::NotRegistered(_))
        ));
    }

    #[test]
    fn wait_for_state_accepts_and_times_out() {
        let fake = FakeControl::new(vec![
            st(ServiceState::StopPending, 0, 0),
            st(ServiceState::NotInstalled, 0, 0),
        ]);
        let s = wait_for_state_polling(&fake, "svc", &[ServiceState::NotInstalled], LONG, FAST, &mut |_| {})
            .unwrap();
        assert_eq!(s.state, ServiceState::NotInstalled);
        let fake = FakeControl::new(vec![st(ServiceState::Running, 0, 0)]);
        let err = wait_for_state_polling(
            &fake,
            "svc",
            &[ServiceState::NotInstalled],
            Duration::from_millis(10),
            FAST,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("not_installed"), "{err}");
    }

    #[test]
    fn old_definition_json_without_new_fields_deserializes() {
        let json = r#"{
            "version": 1,
            "name": "llm-backend-planner",
            "display_name": "LLM Backend planner",
            "description": "d",
            "program": "C:\\llama-server.exe",
            "args": [],
            "env": [],
            "workdir": null,
            "stdout_log": "C:\\data\\logs\\backend-planner.stdout.log",
            "stderr_log": "C:\\data\\logs\\backend-planner.stderr.log",
            "pid_file": null,
            "account": null,
            "start_mode": "manual",
            "failure_actions": {
                "first_delay_secs": 10,
                "second_delay_secs": 30,
                "subsequent_delay_secs": 60,
                "reset_period_secs": 3600
            }
        }"#;
        let def: ServiceDefinition = serde_json::from_str(json).expect("old definition");
        assert_eq!(def.lifecycle_log, None);
        assert!(!def.failure_actions.restart_on_child_exit);
        // The marker log is derived from the stdout log for it.
        assert_eq!(
            lifecycle_log_path(&def),
            Some(PathBuf::from("C:\\data\\logs\\backend-planner.stdout.log").with_file_name("backend-planner.lifecycle.log"))
        );
        // An old status payload without the specific code, too.
        let s: ServiceStatus = serde_json::from_str(
            r#"{"name":"x","state":"stopped","pid":null,"child_pid":null,"win32_exit_code":0}"#,
        )
        .expect("old status");
        assert_eq!(s.service_specific_exit_code, 0);
    }

    #[test]
    fn definitions_carry_lifecycle_logs_and_child_exit_policy() {
        let logs = Path::new(r"C:\data\logs");
        let def = backend_definition(&sample_spec(), logs);
        assert_eq!(def.pid_file, Some(logs.join("backend-planner.pid")));
        assert_eq!(def.lifecycle_log, Some(logs.join("backend-planner.lifecycle.log")));
        assert!(!def.failure_actions.restart_on_child_exit);

        let gw = gateway_definition(
            LaunchSpec {
                program: PathBuf::from("python.exe"),
                args: Vec::new(),
                env: Vec::new(),
                workdir: None,
            },
            logs.join("gateway.stdout.log"),
            logs.join("gateway.stderr.log"),
            PathBuf::from(r"C:\data\gateway.pid"),
        );
        assert_eq!(gw.lifecycle_log, Some(logs.join("gateway.lifecycle.log")));
        assert!(!gw.failure_actions.restart_on_child_exit);

        let data_dir = Path::new(r"C:\data");
        let lr = log_receiver_definition(data_dir, Path::new("recv.exe"));
        assert_eq!(
            lr.lifecycle_log,
            Some(log_receiver_dir(data_dir).join("logs").join("receiver.lifecycle.log"))
        );
        assert!(lr.failure_actions.restart_on_child_exit);
        assert_eq!(lifecycle_log_path(&lr), lr.lifecycle_log);
    }

    #[test]
    fn lifecycle_keys_match_the_app_vocabulary() {
        assert_eq!(lifecycle_key(GATEWAY_SERVICE_NAME), "gateway");
        assert_eq!(lifecycle_key(LOG_RECEIVER_SERVICE_NAME), "log-receiver");
        assert_eq!(lifecycle_key(&backend_service_name("coder")), "backend:coder");
        assert_eq!(lifecycle_key("other"), "other");
    }
}
