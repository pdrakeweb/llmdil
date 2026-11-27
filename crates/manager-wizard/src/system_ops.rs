//! The injectable `SystemOps` trait every system effect goes through.

use super::*;

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
    /// Start a registered Windows service (service supervision mode;
    /// see manager-services and docs/windows-services-design.md) and
    /// return the pid of the service's child process — read from the
    /// definition's pid file once the service host has written it, so
    /// the gateway step's pid-based health checks work unchanged.
    /// The definition file itself is written by the caller
    /// (`manager_services::write_definition`) before this runs.
    /// The default implementation is unsupported; the real one lives
    /// in [`ops::RealSystemOps`] on Windows.
    async fn service_start_gateway(
        &self,
        def: &manager_services::ServiceDefinition,
    ) -> Result<u32, WizardError> {
        Err(WizardError::IoError {
            path: PathBuf::from("scm"),
            reason: format!(
                "service supervision is not supported by this system-ops implementation (service '{}')",
                def.name
            ),
        })
    }
    /// Stop a registered Windows service and wait until it reaches
    /// the stopped state (the shared
    /// [`manager_services::stop_and_wait`]). The typed outcome says
    /// what actually happened — `NotRegistered` (nothing to stop),
    /// `AlreadyStopped`, or `Stopped { waited_ms }` — so callers log
    /// honestly instead of always claiming a stop.
    async fn service_stop(&self, name: &str) -> Result<manager_services::StopOutcome, WizardError> {
        Err(WizardError::IoError {
            path: PathBuf::from("scm"),
            reason: format!(
                "service supervision is not supported by this system-ops implementation (service '{name}')"
            ),
        })
    }
    /// Persist a service definition (the file the service host reads
    /// at start) under `<data_dir>/services/`, returning its path. The
    /// default writes it atomically via
    /// [`manager_services::write_definition`]; the test double keeps
    /// it in its fake filesystem.
    async fn write_service_definition(
        &self,
        data_dir: &Path,
        def: &manager_services::ServiceDefinition,
    ) -> Result<PathBuf, WizardError> {
        manager_services::write_definition(data_dir, def).map_err(|e| WizardError::IoError {
            path: manager_services::definition_path(data_dir, &def.name),
            reason: e.to_string(),
        })
    }
    /// Whether a Windows service named `name` is registered with the
    /// SCM. Used to route the gateway's start/stop: service mode only
    /// applies when the gateway service actually exists (the same
    /// "mode AND registered" rule the backends use). A query failure
    /// reads as not registered. The default reports nothing
    /// registered.
    async fn service_registered(&self, _name: &str) -> bool {
        false
    }
    /// The SCM state of the service `name`, when it can be queried
    /// (`None`: not supported / query failed). Used by the gateway's
    /// final health re-check in service mode.
    async fn service_state(&self, _name: &str) -> Option<manager_services::ServiceState> {
        None
    }
    /// Append one lifecycle marker line (see
    /// [`manager_supervisor::lifecycle::marker_line`]) to a service's
    /// lifecycle log. Best-effort: never fails. The default writes
    /// the file (rotating past 1 MiB); the test double records the
    /// call instead.
    fn append_marker(&self, path: &Path, line: &str) {
        manager_supervisor::lifecycle::append_marker(path, line)
    }
    /// The pid owning the LISTEN socket on `port`, when the platform
    /// can tell (Windows TCP table). Used to find a gateway that is
    /// still serving although no pid file records it (a pid file
    /// lost to a mode flip, a crash in the spawn/pid-write window,
    /// an older build). The default cannot tell.
    fn port_owner_pid(&self, _port: u16) -> Option<u32> {
        None
    }
    /// Whether a process with `pid` is currently alive.
    fn process_alive(&self, pid: u32) -> bool;
    /// Whether the process with `pid` is an instance of the named
    /// executable image (e.g. `"postgres.exe"`), when the platform
    /// can say: `Some(true)` / `Some(false)` are determinations,
    /// `None` is "cannot tell" — callers must treat `None`
    /// conservatively, exactly like a match. Used to tell a live
    /// postmaster apart from an unrelated process that recycled
    /// its pid (the `postmaster.pid` inspection in
    /// `steps::ensure_postgres_running`). The default implementation
    /// cannot tell.
    fn process_image_is(&self, _pid: u32, _image: &str) -> Option<bool> {
        None
    }
    /// Whether `pid` is (still) a gateway interpreter process — the
    /// guard before force-killing a pid read from `gateway.pid`
    /// (finding PL-M4, `docs/code-review-v0.1.60.md`). Same
    /// tri-state as [`Self::process_image_is`]: only `Some(false)`
    /// (a live process of another image, i.e. a recycled pid) means
    /// "do not kill". The default checks the Windows image name.
    fn process_is_gateway(&self, pid: u32) -> Option<bool> {
        self.process_image_is(pid, crate::ops::GATEWAY_PROCESS_IMAGE)
    }
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
    /// Check a Windows Credential Manager target exists (with a non-empty
    /// value). Presence only — the value is never returned.
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
