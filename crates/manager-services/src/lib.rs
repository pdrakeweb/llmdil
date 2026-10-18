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

use manager_supervisor::{ProcessSpec, ProcessState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Definition format version written by [`write_definition`].
pub const DEFINITION_VERSION: u32 = 1;

/// Service name of the gateway service.
pub const GATEWAY_SERVICE_NAME: &str = "llm-gateway";

/// Credential target the app uses to hand the service account's
/// password to one elevated `register` run of `llm-service-host`.
/// Written by the Settings register action, read and deleted by the
/// helper; the durable copy of the password is the SCM's LSA secret.
pub const SERVICE_ACCOUNT_PASSWORD_CRED_TARGET: &str =
    "local-llm-service-manager/service-account-password";

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
    pub fn literal(name: &str, value: String) -> Self {
        Self {
            name: name.to_string(),
            value: EnvValue::Literal { value },
        }
    }

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureActions {
    pub first_delay_secs: u32,
    pub second_delay_secs: u32,
    pub subsequent_delay_secs: u32,
    pub reset_period_secs: u32,
}

impl Default for FailureActions {
    fn default() -> Self {
        Self {
            first_delay_secs: 10,
            second_delay_secs: 30,
            subsequent_delay_secs: 60,
            reset_period_secs: 3600,
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
    /// The real program the shim spawns (llama-server, pythonw).
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
        pid_file: None,
        account: None,
        start_mode: ServiceStartMode::Manual,
        failure_actions: FailureActions::default(),
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
    /// [`scm::query`] callers that know the definition; the SCM itself
    /// only reports the service's own pid.
    pub child_pid: Option<u32>,
    pub win32_exit_code: u32,
}

impl ServiceStatus {
    /// Map onto the supervisor's backend state model. `NotInstalled`
    /// maps to `Stopped` — callers distinguish it via
    /// [`ServiceState`] before mapping.
    pub fn process_state(&self) -> ProcessState {
        match self.state {
            ServiceState::Running => ProcessState::Running,
            ServiceState::StartPending => ProcessState::Starting,
            ServiceState::StopPending => ProcessState::Stopping,
            ServiceState::Paused => ProcessState::Failed,
            ServiceState::Stopped
            | ServiceState::NotInstalled
            | ServiceState::Unknown(_) => ProcessState::Stopped,
        }
    }
}

/// Operations the app performs against the SCM. Implemented for real
/// on Windows ([`scm::WindowsServiceControl`]); elsewhere
/// ([`scm::UnsupportedServiceControl`]) every operation fails with
/// [`ServiceError::Unsupported`].
pub trait ServiceControl: Send + Sync {
    /// Status of one service; `NotInstalled` is a state, not an error.
    fn query(&self, name: &str) -> Result<ServiceStatus, ServiceError>;
    fn is_registered(&self, name: &str) -> Result<bool, ServiceError> {
        Ok(!matches!(self.query(name)?.state, ServiceState::NotInstalled))
    }
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
    fn scm_states_map_onto_the_backend_model() {
        let status = |state| ServiceStatus {
            name: "llm-backend-planner".to_string(),
            state,
            pid: None,
            child_pid: None,
            win32_exit_code: 0,
        };
        assert_eq!(status(ServiceState::Running).process_state(), ProcessState::Running);
        assert_eq!(status(ServiceState::StartPending).process_state(), ProcessState::Starting);
        assert_eq!(status(ServiceState::StopPending).process_state(), ProcessState::Stopping);
        assert_eq!(status(ServiceState::Stopped).process_state(), ProcessState::Stopped);
        assert_eq!(status(ServiceState::NotInstalled).process_state(), ProcessState::Stopped);
    }
}
