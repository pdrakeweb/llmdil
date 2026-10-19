//! Process supervision (spec §2.2, §6).
//!
//! The app supervises child processes — one `llama-server` per backend, the
//! LiteLLM gateway, local MCP servers, optionally WinMLServer — and never
//! embeds inference. The supervisor owns spawn args, health probing, graceful
//! drain (30 s default, then kill), exponential crash backoff (capped), and
//! stdout/stderr capture into per-scope ring buffers feeding the log bus.
//!
//! Design notes:
//! - `SupervisedProcess` is a single live child. `Supervisor` owns the
//!   registry (`ProcessSpec` per id), the live map, crash budgets, health
//!   probes and 3-strike/2-pass health trackers.
//! - `drain()` returns `Ok` once the process is stopped — including when the
//!   graceful window expired and the process had to be killed. The desired end
//!   state (stopped) is what the caller asked for; a note is pushed to the
//!   log ring when the kill path was taken. `DrainTimeout` is reserved for the
//!   pathological case where even the kill fails.
//! - Graceful signal is SIGTERM on Unix. On Windows v1 there is no console-
//!   control delivery wired yet, so `drain()` waits the timeout for natural
//!   exit and then kills (future work: GenerateConsoleCtrlEvent).
//! - Children are spawned with `kill_on_drop(true)`: dropping a
//!   `SupervisedProcess` without an explicit stop still reaps the child.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// Graceful-drain timeout used by `Supervisor::stop(id, graceful = true)`
/// (spec §2.2: 30 s, then kill).
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for reaping a child after the kill in `drain()`. If the
/// child is still alive past this, `drain()` returns `DrainTimeout`
/// instead of blocking the supervisor forever.
pub const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Default ring-buffer capacity per supervised process (spec §2.4: 10k lines).
pub const DEFAULT_LOG_RING_CAPACITY: usize = 10_000;

/// Lifecycle state per backend (spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

/// Health-probe result for a supervised process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

/// Pluggable health probe: HTTP endpoint, port-open check, etc.
#[async_trait]
pub trait HealthProbe: Send + Sync {
    async fn probe(&self) -> Result<HealthStatus, SupervisorError>;
}

/// HTTP health probe (e.g. `GET http://127.0.0.1:8081/health`).
///
/// 2xx → Healthy; other status codes → Degraded; connection errors and
/// timeouts → Unhealthy.
#[derive(Debug, Clone)]
pub struct HttpHealthProbe {
    pub url: String,
    pub timeout: Duration,
    client: reqwest::Client,
}

impl HttpHealthProbe {
    pub fn new(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            url: url.into(),
            timeout,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl HealthProbe for HttpHealthProbe {
    async fn probe(&self) -> Result<HealthStatus, SupervisorError> {
        match tokio::time::timeout(self.timeout, self.client.get(&self.url).send()).await {
            Err(_) => Ok(HealthStatus::Unhealthy(format!(
                "probe timed out after {:?}",
                self.timeout
            ))),
            Ok(Err(e)) => Ok(HealthStatus::Unhealthy(format!(
                "probe request failed: {e}"
            ))),
            Ok(Ok(resp)) => {
                let status = resp.status();
                if status.is_success() {
                    Ok(HealthStatus::Healthy)
                } else {
                    Ok(HealthStatus::Degraded(format!("HTTP {}", status.as_u16())))
                }
            }
        }
    }
}

/// 3-strike / 2-pass flapping guard (spec §6.2): 3 consecutive failures move
/// the displayed status to the failure; recovery requires 2 consecutive
/// passes. Intermediate observations keep the previous displayed status so
/// the UI does not flap.
#[derive(Debug, Clone)]
pub struct HealthTracker {
    consecutive_failures: u32,
    consecutive_passes: u32,
    displayed: HealthStatus,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            consecutive_failures: 0,
            consecutive_passes: 0,
            displayed: HealthStatus::Healthy,
        }
    }

    /// Feed one probe result; returns the status the UI should display.
    pub fn observe(&mut self, result: HealthStatus) -> HealthStatus {
        match &result {
            HealthStatus::Healthy => {
                self.consecutive_failures = 0;
                self.consecutive_passes += 1;
                if self.consecutive_passes >= 2 {
                    self.displayed = HealthStatus::Healthy;
                }
            }
            other => {
                self.consecutive_passes = 0;
                self.consecutive_failures += 1;
                if self.consecutive_failures >= 3 {
                    self.displayed = other.clone();
                }
            }
        }
        self.displayed.clone()
    }

    pub fn displayed(&self) -> &HealthStatus {
        &self.displayed
    }
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Exponential backoff policy for crash restarts.
#[derive(Debug, Clone)]
pub struct BackoffPolicy {
    pub base: Duration,
    pub max: Duration,
    pub max_retries: u32,
}

impl BackoffPolicy {
    /// Delay before retry number `attempt` (0-based), capped at `max`.
    /// The shift saturates so large attempt counts cannot overflow.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let shift = attempt.min(10);
        self.base.saturating_mul(1u32 << shift).min(self.max)
    }

    /// `true` while another restart is allowed.
    pub fn allows_retry(&self, attempt: u32) -> bool {
        attempt < self.max_retries
    }
}

/// Which stream a captured line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// One captured log line, feeding the log bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    pub ts: DateTime<Utc>,
    pub stream: LogStream,
    pub line: String,
    /// Process id that produced the line. Empty at capture time — the
    /// stream readers do not know their ring's key — and stamped from the
    /// ring key by [`Supervisor::logs_query`] on every record it returns,
    /// so a merged "all" view can attribute each line to its process.
    pub source: String,
    /// True when the record comes from the archived previous-run ring of
    /// its process rather than the current ring. Stamped at query time,
    /// like `source`.
    pub archived: bool,
}

impl LogRecord {
    pub fn new(stream: LogStream, line: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            stream,
            line: line.into(),
            source: String::new(),
            archived: false,
        }
    }
}

/// Fixed-capacity ring buffer (default 10k lines per scope, spec §2.4).
/// When full, the oldest record is evicted on push.
#[derive(Debug)]
pub struct RingBuffer<T> {
    buf: VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.buf.len() >= self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Newest-first view for UI queries.
    pub fn recent(&self, limit: usize) -> Vec<&T> {
        self.buf.iter().rev().take(limit).collect()
    }
}

/// Static launch description for one supervised process. The supervisor
/// spawns exactly `program + args` with `env`/`workdir`; `launch_argv()`
/// exposes the same argv the wizard's "Show commands" displays.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<PathBuf>,
    /// Optional TCP port checked for conflicts before spawn (spec §6.2).
    pub port: Option<u16>,
}

impl ProcessSpec {
    /// Full argv: `[program, args...]`.
    pub fn argv(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(self.args.len() + 1);
        v.push(self.program.to_string_lossy().into_owned());
        v.extend(self.args.iter().cloned());
        v
    }
}

/// Point-in-time snapshot of one registered process for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub id: String,
    pub state: ProcessState,
    pub pid: Option<u32>,
    /// True when the process was adopted (started outside this app
    /// session — e.g. by a previous launch — and taken over by pid).
    pub adopted: bool,
    /// Crash-restart attempts since the last healthy observation or manual
    /// (re)start.
    pub restarts: u32,
    pub uptime: Option<Duration>,
}

/// Send the graceful-termination signal. Unix: SIGTERM. Windows v1: no-op —
/// `drain()` still waits the timeout for natural exit before killing.
fn send_graceful_signal(child: &Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

/// Max bytes accumulated for one captured log line (64 KiB). A supervised
/// process emitting a never-terminated line must not grow the manager's
/// memory without bound: past the cap the accumulated line is emitted
/// with a truncation marker and the rest of the line is discarded.
pub const MAX_LOG_LINE_BYTES: usize = 64 * 1024;

/// Append `bytes` to the line being accumulated, capping the total at
/// [`MAX_LOG_LINE_BYTES`]. Sets `truncated` when any byte had to be
/// dropped; once set, further appends discard until the line ends.
fn append_capped(line: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    if *truncated {
        return;
    }
    let room = MAX_LOG_LINE_BYTES.saturating_sub(line.len());
    if bytes.len() > room {
        line.extend_from_slice(&bytes[..room]);
        *truncated = true;
    } else {
        line.extend_from_slice(bytes);
    }
}

/// Push one completed captured line into the ring. Lossy UTF-8 (a
/// non-UTF8 byte sequence must not kill capture), trailing CR stripped
/// (the LF was already consumed by the splitter), truncation marked.
fn push_captured_line(
    ring: &Arc<Mutex<RingBuffer<LogRecord>>>,
    which: LogStream,
    bytes: &[u8],
    truncated: bool,
) {
    let text = String::from_utf8_lossy(bytes);
    let line = text.strip_suffix('\r').unwrap_or(&text);
    let line = if truncated {
        format!("{line} [truncated: line exceeded the 64 KiB capture cap]")
    } else {
        line.to_string()
    };
    if let Ok(mut r) = ring.lock() {
        r.push(LogRecord::new(which, line));
    }
}

fn spawn_reader<R>(
    stream: R,
    which: LogStream,
    ring: Arc<Mutex<RingBuffer<LogRecord>>>,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        // Fixed-size chunk reads, split on LF manually: read_until would
        // accumulate a whole unterminated line in memory before returning
        // anything, which is exactly the unbounded growth the cap exists
        // to prevent. The accumulated line never exceeds
        // MAX_LOG_LINE_BYTES.
        let mut reader = BufReader::new(stream);
        let mut line: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let data = &chunk[..n];
                    let mut start = 0usize;
                    for (i, &b) in data.iter().enumerate() {
                        if b == b'\n' {
                            append_capped(&mut line, &data[start..i], &mut truncated);
                            push_captured_line(&ring, which, &line, truncated);
                            line.clear();
                            truncated = false;
                            start = i + 1;
                        }
                    }
                    append_capped(&mut line, &data[start..], &mut truncated);
                }
                Err(_) => break,
            }
        }
        // EOF with an unterminated tail: emit what was captured (the
        // previous read_until loop did the same via its final read).
        if !line.is_empty() || truncated {
            push_captured_line(&ring, which, &line, truncated);
        }
    })
}

/// A supervised child process: one live OS process with captured output.
pub struct SupervisedProcess {
    pub id: String,
    child: Option<Child>,
    /// Pid of a process this supervisor did not spawn but adopted
    /// (see [`SupervisedProcess::adopt`]); mutually exclusive with
    /// `child`.
    adopted_pid: Option<u32>,
    readers: Vec<JoinHandle<()>>,
    log_ring: Arc<Mutex<RingBuffer<LogRecord>>>,
    state: ProcessState,
    restart_attempts: u32,
    started_at: Option<Instant>,
    last_exit: Option<std::process::ExitStatus>,
}

impl std::fmt::Debug for SupervisedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisedProcess")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("pid", &self.pid())
            .field("restart_attempts", &self.restart_attempts)
            .finish()
    }
}

impl SupervisedProcess {
    /// Spawn `program` with `args`/`env`, capturing stdout/stderr into the
    /// shared ring buffer. The child is `kill_on_drop`: dropping this struct
    /// without an explicit stop still reaps the child.
    pub async fn spawn(
        id: &str,
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        log_ring: Arc<Mutex<RingBuffer<LogRecord>>>,
    ) -> Result<Self, SupervisorError> {
        Self::spawn_with_workdir(id, program, args, env, None, log_ring).await
    }

    /// `spawn` plus an explicit working directory.
    pub async fn spawn_with_workdir(
        id: &str,
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        workdir: Option<&Path>,
        log_ring: Arc<Mutex<RingBuffer<LogRecord>>>,
    ) -> Result<Self, SupervisorError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(w) = workdir {
            cmd.current_dir(w);
        }
        // Backends are headless: stdout/stderr are piped into the log
        // store below. On Windows a console child spawned from the
        // GUI app would otherwise open a visible console window.
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW. `cmd` is a tokio::process::Command,
            // whose creation_flags is an inherent method on Windows —
            // no std CommandExt import is needed (or used) here.
            cmd.creation_flags(0x08000000);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| SupervisorError::SpawnFailed(id.to_string(), e.to_string()))?;
        tracing::info!(process = id, pid = child.id(), "spawned supervised process");

        let mut readers = Vec::with_capacity(2);
        if let Some(out) = child.stdout.take() {
            readers.push(spawn_reader(out, LogStream::Stdout, Arc::clone(&log_ring)));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(spawn_reader(err, LogStream::Stderr, Arc::clone(&log_ring)));
        }

        Ok(Self {
            id: id.to_string(),
            child: Some(child),
            adopted_pid: None,
            readers,
            log_ring,
            state: ProcessState::Running,
            restart_attempts: 0,
            started_at: Some(Instant::now()),
            last_exit: None,
        })
    }

    /// Adopt a live process this supervisor did not spawn — a
    /// backend started by a previous app session survives the app
    /// closing (nothing kills it), and the new session must still
    /// see it: status, pid-keyed VRAM attribution, and stop all key
    /// off the tracked pid. There is no captured output (the log
    /// ring starts empty) and no known start time (uptime None).
    pub fn adopt(id: &str, pid: u32, log_ring: Arc<Mutex<RingBuffer<LogRecord>>>) -> Self {
        Self {
            id: id.to_string(),
            child: None,
            adopted_pid: Some(pid),
            readers: Vec::new(),
            log_ring,
            state: ProcessState::Running,
            restart_attempts: 0,
            started_at: None,
            last_exit: None,
        }
    }

    /// For an adopted process: true once the tracked pid is gone.
    /// The state becomes Failed (the exit was not ours), matching
    /// how an unexpected child exit is reported.
    pub fn adopted_exited(&mut self) -> bool {
        match self.adopted_pid {
            Some(pid) if !pid_alive(pid) => {
                self.adopted_pid = None;
                self.state = ProcessState::Failed;
                self.abort_readers();
                true
            }
            _ => false,
        }
    }

    /// Graceful drain: send the graceful signal, wait `timeout` for natural
    /// exit, then kill. Returns `Ok` once the process is stopped either way.
    pub async fn drain(&mut self, timeout: Duration) -> Result<(), SupervisorError> {
        let mut child = match self.child.take() {
            None => {
                if let Some(pid) = self.adopted_pid {
                    // No graceful channel exists to a process we did
                    // not spawn: wait out the window for a natural
                    // exit, then kill the tree.
                    let deadline = Instant::now() + timeout;
                    while pid_alive(pid) && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    if pid_alive(pid) {
                        kill_pid_tree(pid);
                        // Confirm the exit before reporting the stop:
                        // the process holds its listening port until
                        // it is actually gone.
                        wait_pid_gone(pid, Duration::from_secs(5)).await;
                    }
                    self.adopted_pid = None;
                    self.state = ProcessState::Stopped;
                    self.abort_readers();
                    return Ok(());
                }
                self.state = ProcessState::Stopped;
                return Ok(());
            }
            Some(c) => c,
        };

        // Fast path: already exited.
        match child.try_wait() {
            Ok(Some(status)) => {
                self.record_exit(status, ProcessState::Stopped);
                self.abort_readers();
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                self.child = Some(child);
                return Err(SupervisorError::Io(e));
            }
        }

        self.state = ProcessState::Stopping;
        send_graceful_signal(&child);

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(process = self.id, "drained gracefully");
                self.record_exit(status, ProcessState::Stopped);
                self.abort_readers();
                Ok(())
            }
            Ok(Err(e)) => {
                self.child = Some(child);
                Err(SupervisorError::Io(e))
            }
            Err(_) => {
                // Graceful window expired: kill and reap.
                self.note(
                    LogStream::Stderr,
                    format!("drain timed out after {timeout:?}; killing process"),
                );
                tracing::warn!(process = self.id, "drain timed out; killing");
                let _ = child.kill().await;
                // The kill is best-effort: if the child still will not die,
                // report DrainTimeout instead of hanging the supervisor on
                // wait() forever.
                match tokio::time::timeout(KILL_REAP_TIMEOUT, child.wait()).await {
                    Ok(Ok(status)) => {
                        self.record_exit(status, ProcessState::Stopped);
                        self.abort_readers();
                        Ok(())
                    }
                    _ => {
                        self.record_exit_unknown();
                        self.abort_readers();
                        // `child` drops here; kill_on_drop makes one last
                        // best-effort attempt. The process is still alive,
                        // which is the pathological case DrainTimeout exists for.
                        Err(SupervisorError::DrainTimeout(self.id.clone()))
                    }
                }
            }
        }
    }

    /// Force-kill immediately and reap.
    pub async fn kill(&mut self) -> Result<(), SupervisorError> {
        let mut child = match self.child.take() {
            None => {
                if let Some(pid) = self.adopted_pid.take() {
                    kill_pid_tree(pid);
                    wait_pid_gone(pid, Duration::from_secs(5)).await;
                }
                self.state = ProcessState::Stopped;
                self.abort_readers();
                return Ok(());
            }
            Some(c) => c,
        };
        // May already be dead; ignore the error and reap regardless.
        let _ = child.kill().await;
        match child.wait().await {
            Ok(status) => self.record_exit(status, ProcessState::Stopped),
            Err(_) => self.record_exit_unknown(),
        }
        self.abort_readers();
        tracing::info!(process = self.id, "killed");
        Ok(())
    }

    /// Non-blocking check: reaps the child if it has exited. Returns the
    /// exit status on transition, `None` while still running. An exit
    /// observed here is unexpected (explicit stops remove the process from
    /// the supervisor first), so the state becomes `Failed`.
    pub fn poll_exited(&mut self) -> Result<Option<std::process::ExitStatus>, SupervisorError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                self.last_exit = Some(status);
                self.state = ProcessState::Failed;
                self.abort_readers();
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(SupervisorError::Io(e)),
        }
    }

    /// Record a crash against the backoff policy. Returns the delay before
    /// the next restart attempt, or `RestartBudgetExhausted`.
    pub fn note_crash(&mut self, backoff: &BackoffPolicy) -> Result<Duration, SupervisorError> {
        if !backoff.allows_retry(self.restart_attempts) {
            return Err(SupervisorError::RestartBudgetExhausted(self.id.clone()));
        }
        let delay = backoff.delay_for_attempt(self.restart_attempts);
        self.restart_attempts += 1;
        Ok(delay)
    }

    /// Reset the crash budget after a healthy observation.
    pub fn note_healthy(&mut self) {
        self.restart_attempts = 0;
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn state(&self) -> ProcessState {
        self.state
    }

    pub fn pid(&self) -> Option<u32> {
        self.child
            .as_ref()
            .and_then(|c| c.id())
            .or(self.adopted_pid)
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some() || self.adopted_pid.is_some()
    }

    pub fn uptime(&self) -> Option<Duration> {
        self.started_at.map(|t| t.elapsed())
    }

    pub fn restart_attempts(&self) -> u32 {
        self.restart_attempts
    }

    pub fn last_exit(&self) -> Option<std::process::ExitStatus> {
        self.last_exit
    }

    pub fn log_ring(&self) -> Arc<Mutex<RingBuffer<LogRecord>>> {
        Arc::clone(&self.log_ring)
    }

    fn record_exit(&mut self, status: std::process::ExitStatus, state: ProcessState) {
        self.last_exit = Some(status);
        self.state = state;
        self.started_at = None;
    }

    fn record_exit_unknown(&mut self) {
        self.state = ProcessState::Stopped;
        self.started_at = None;
    }

    fn abort_readers(&mut self) {
        for h in self.readers.drain(..) {
            h.abort();
        }
    }

    fn note(&self, stream: LogStream, line: String) {
        if let Ok(mut r) = self.log_ring.lock() {
            r.push(LogRecord::new(stream, line));
        }
    }
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Wait until `pid` no longer exists, polling briefly (false on
/// timeout). `kill_pid_tree` only issues the kill — taskkill on
/// Windows is fire-and-forget — so callers that stop an adopted
/// process confirm its exit here before reporting the stop complete.
async fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

/// Wait until `port` can be bound, polling briefly. A process that
/// was just killed — an adopted backend especially, whose kill is
/// fire-and-forget — can hold its listener for a short while after
/// the stop returns; a restart that probes the port once in that
/// window fails with PortInUse even though nothing is wrong (Pete's
/// restart failures, 2026-10-15). Returns as soon as the port is
/// free; false when the timeout expires with it still held.
async fn wait_port_free(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if port_free(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether a process with `pid` currently exists.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        ok != 0 && code == STILL_ACTIVE
    }
    #[cfg(unix)]
    {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}

/// Kill a process and (on Windows) its whole tree; best effort —
/// used for adopted processes, which have no child handle.
fn kill_pid_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            // CREATE_NO_WINDOW: no console flash from the GUI app.
            .creation_flags(0x08000000)
            .output();
    }
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        let _ = kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// The pid owning the LISTEN socket on `port`, when one exists.
/// Windows: the TCP table via iphlpapi (the same data `netstat
/// -ano` prints). Other platforms: not resolved (None) — adoption
/// is a Windows-path feature for now.
pub fn port_owner_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    {
        windows_port_owner_pid(port)
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        None
    }
}

#[cfg(windows)]
fn windows_port_owner_pid(port: u16) -> Option<u32> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    const AF_INET: u32 = 2;
    // MIB_TCP_STATE_LISTEN
    const LISTEN: u32 = 2;
    let mut size: u32 = 0;
    let rc = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if rc != 122 || size == 0 {
        // ERROR_INSUFFICIENT_BUFFER (122) is the expected first call.
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let rc = unsafe {
        GetExtendedTcpTable(
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let count = u32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    let row_size = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
    for i in 0..count {
        let off = 4 + i * row_size;
        let row_bytes = buf.get(off..off + row_size)?;
        let row: MIB_TCPROW_OWNER_PID =
            unsafe { std::ptr::read_unaligned(row_bytes.as_ptr() as *const MIB_TCPROW_OWNER_PID) };
        // Ports are stored in network byte order in the low 16 bits.
        let local_port = u16::from_be((row.dwLocalPort & 0xffff) as u16);
        if local_port == port && row.dwState == LISTEN {
            return Some(row.dwOwningPid);
        }
    }
    None
}

/// Whether `pid`'s executable file name equals `name`
/// (case-insensitive) — the guard against adopting whatever
/// unrelated program happens to sit on a backend's port.
pub fn process_image_is(pid: u32, name: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        if ok == 0 {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .map(|f| f.eq_ignore_ascii_case(name))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, name);
        false
    }
}

/// Owns all supervised processes: llama-servers, LiteLLM, MCP servers, WinML.
pub struct Supervisor {
    specs: HashMap<String, ProcessSpec>,
    live: HashMap<String, SupervisedProcess>,
    /// Retained log rings of stopped/reaped processes so `logs_query` keeps
    /// working after a process exits. Moved to `archived_rings` on the next
    /// spawn of the id.
    dead_rings: HashMap<String, Arc<Mutex<RingBuffer<LogRecord>>>>,
    /// One archived previous-run ring per id: when an id is spawned again,
    /// its retained ring moves here (replacing any older archive) so the
    /// previous run's output — usually the failure being recovered from —
    /// survives the respawn. Records from these rings are returned by
    /// `logs_query` flagged `archived`.
    archived_rings: HashMap<String, Arc<Mutex<RingBuffer<LogRecord>>>>,
    probes: HashMap<String, Arc<dyn HealthProbe>>,
    trackers: HashMap<String, HealthTracker>,
    failed_ids: HashSet<String>,
    crash_counts: HashMap<String, u32>,
    backoff: BackoffPolicy,
}

impl Supervisor {
    pub fn new(backoff: BackoffPolicy) -> Self {
        Self {
            specs: HashMap::new(),
            live: HashMap::new(),
            dead_rings: HashMap::new(),
            archived_rings: HashMap::new(),
            probes: HashMap::new(),
            trackers: HashMap::new(),
            failed_ids: HashSet::new(),
            crash_counts: HashMap::new(),
            backoff,
        }
    }

    pub fn backoff(&self) -> &BackoffPolicy {
        &self.backoff
    }

    /// Register (or replace) the launch spec for `id`. The spec is what
    /// `spawn_backend` spawns and what `launch_argv` reports.
    pub fn register_spec(&mut self, spec: ProcessSpec) {
        self.specs.insert(spec.id.clone(), spec);
    }

    /// The registered spec for `id`, if any.
    pub fn spec(&self, id: &str) -> Option<&ProcessSpec> {
        self.specs.get(id)
    }

    /// Remove the spec for `id`. Refused (returns false) while a process
    /// with that id is live — a running process keeps its spec so status
    /// and stop keep working after the id leaves the configuration.
    pub fn unregister_spec(&mut self, id: &str) -> bool {
        if self.live.contains_key(id) {
            return false;
        }
        let removed = self.specs.remove(id).is_some();
        if removed {
            // Drop the id's runtime history too: a config entry later
            // re-added under the same id must start clean, not inherit a
            // stale "failed" state and restart count.
            self.failed_ids.remove(id);
            self.crash_counts.remove(id);
            self.trackers.remove(id);
            self.dead_rings.remove(id);
            self.archived_rings.remove(id);
        }
        removed
    }

    pub fn spec_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Attach a health probe for `id`.
    pub fn register_probe(&mut self, id: &str, probe: Arc<dyn HealthProbe>) {
        self.probes.insert(id.to_string(), probe);
    }

    /// Spawn the registered backend `id` (spec §6.2: port-conflict check
    /// before spawn). A manual spawn resets the crash budget for `id`.
    pub async fn spawn_backend(&mut self, id: &str) -> Result<(), SupervisorError> {
        if self.live.contains_key(id) {
            return Err(SupervisorError::AlreadyRunning(id.to_string()));
        }
        let spec = self
            .specs
            .get(id)
            .ok_or_else(|| SupervisorError::NotFound(id.to_string()))?
            .clone();
        if let Some(port) = spec.port {
            // Bounded wait, not a single probe: a just-stopped
            // process (above all an adopted one) may still be
            // releasing this port. A port held by something that is
            // not going away fails after the wait, with the same
            // PortInUse error as before.
            if !wait_port_free(port, Duration::from_secs(10)).await {
                return Err(SupervisorError::PortInUse(port));
            }
        }
        let ring = Arc::new(Mutex::new(RingBuffer::new(DEFAULT_LOG_RING_CAPACITY)));
        let proc = SupervisedProcess::spawn_with_workdir(
            id,
            &spec.program,
            &spec.args,
            &spec.env,
            spec.workdir.as_deref(),
            ring,
        )
        .await?;
        self.live.insert(id.to_string(), proc);
        // Archive the previous run's retained ring before the fresh ring
        // takes over the id: restarting a failed backend is the standard
        // recovery action, and it must not destroy the run that failed.
        // One generation only — an existing archive is replaced.
        if let Some(previous) = self.dead_rings.remove(id) {
            self.archived_rings.insert(id.to_string(), previous);
        }
        self.failed_ids.remove(id);
        self.crash_counts.insert(id.to_string(), 0);
        self.trackers.entry(id.to_string()).or_default();
        Ok(())
    }

    /// Adopt a live external process as `id`'s backend (started by
    /// a previous app session and still holding its port and VRAM).
    /// Fails like `spawn_backend` when the id is already live or has
    /// no registered spec.
    pub fn adopt(&mut self, id: &str, pid: u32) -> Result<(), SupervisorError> {
        if self.live.contains_key(id) {
            return Err(SupervisorError::AlreadyRunning(id.to_string()));
        }
        if !self.specs.contains_key(id) {
            return Err(SupervisorError::NotFound(id.to_string()));
        }
        let ring = Arc::new(Mutex::new(RingBuffer::new(DEFAULT_LOG_RING_CAPACITY)));
        let proc = SupervisedProcess::adopt(id, pid, ring);
        self.live.insert(id.to_string(), proc);
        if let Some(previous) = self.dead_rings.remove(id) {
            self.archived_rings.insert(id.to_string(), previous);
        }
        self.failed_ids.remove(id);
        self.trackers.entry(id.to_string()).or_default();
        Ok(())
    }

    /// Stop a process; graceful drain first when `graceful` is true.
    /// The process's log ring is retained so `logs_query` still serves its
    /// history; when the id is spawned again the ring becomes the archived
    /// previous run instead of being dropped.
    pub async fn stop(&mut self, id: &str, graceful: bool) -> Result<(), SupervisorError> {
        let mut proc = self
            .live
            .remove(id)
            .ok_or_else(|| SupervisorError::NotFound(id.to_string()))?;
        let stop_result = if graceful {
            // On Windows the graceful signal is a no-op (see
            // send_graceful_signal), so a long drain window is pure
            // latency: the child either exits on its own almost
            // immediately or never will. Use a short window there.
            #[cfg(windows)]
            let drain_timeout = Duration::from_secs(3);
            #[cfg(not(windows))]
            let drain_timeout = DEFAULT_DRAIN_TIMEOUT;
            proc.drain(drain_timeout).await
        } else {
            proc.kill().await
        };
        self.dead_rings.insert(id.to_string(), proc.log_ring());
        if let Err(e) = stop_result {
            // The child could not be confirmed dead (e.g. DrainTimeout):
            // it may still be alive and holding its port. Report the id
            // as failed rather than stopped so the state stays honest.
            self.failed_ids.insert(id.to_string());
            return Err(e);
        }
        Ok(())
    }

    /// Graceful stop then spawn again. Unknown ids are a fresh start.
    pub async fn restart(&mut self, id: &str, graceful: bool) -> Result<(), SupervisorError> {
        if self.live.contains_key(id) {
            self.stop(id, graceful).await?;
        }
        self.spawn_backend(id).await
    }

    /// Non-blocking reap of unexpectedly exited children. Returns
    /// `(id, exit_status)` for each; reaped ids show as `Failed` in
    /// `status()` until respawned. Callers apply `crash_delay()` for
    /// backoff before respawning.
    pub fn reap_exited(&mut self) -> Vec<(String, std::process::ExitStatus)> {
        let mut dead = Vec::new();
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            // Adopted processes have no ExitStatus to reap; their
            // deaths are detected by pid liveness and reported the
            // same way (removed from live, marked failed), just
            // without an entry in the returned status list.
            let adopted_dead = self
                .live
                .get_mut(&id)
                .map(|p| p.adopted_exited())
                .unwrap_or(false);
            if adopted_dead {
                if let Some(proc) = self.live.remove(&id) {
                    self.dead_rings.insert(id.clone(), proc.log_ring());
                }
                self.failed_ids.insert(id.clone());
                tracing::warn!(process = id, "adopted process exited unexpectedly");
                continue;
            }
            let exited = self
                .live
                .get_mut(&id)
                .and_then(|p| p.poll_exited().ok().flatten());
            if let Some(status) = exited {
                if let Some(proc) = self.live.remove(&id) {
                    self.dead_rings.insert(id.clone(), proc.log_ring());
                }
                self.failed_ids.insert(id.clone());
                tracing::warn!(
                    process = id,
                    ?status,
                    "supervised process exited unexpectedly"
                );
                dead.push((id, status));
            }
        }
        dead
    }

    /// Crash-budget accounting for `id`: returns the backoff delay before the
    /// next restart, or `RestartBudgetExhausted`. Call `note_healthy(id)`
    /// when the process is observed healthy to reset the budget.
    pub fn crash_delay(&mut self, id: &str) -> Result<Duration, SupervisorError> {
        if !self.specs.contains_key(id) {
            return Err(SupervisorError::NotFound(id.to_string()));
        }
        let attempts = self.crash_counts.get(id).copied().unwrap_or(0);
        if !self.backoff.allows_retry(attempts) {
            return Err(SupervisorError::RestartBudgetExhausted(id.to_string()));
        }
        self.crash_counts.insert(id.to_string(), attempts + 1);
        Ok(self.backoff.delay_for_attempt(attempts))
    }

    /// Reset the crash budget for `id` after a healthy observation.
    pub fn note_healthy(&mut self, id: &str) {
        self.crash_counts.insert(id.to_string(), 0);
    }

    /// Raw probe call for `id` (no flapping guard).
    pub async fn probe(&self, id: &str) -> Result<HealthStatus, SupervisorError> {
        let probe = self
            .probes
            .get(id)
            .ok_or_else(|| SupervisorError::NotFound(format!("no probe registered for {id}")))?;
        probe.probe().await
    }

    /// Probe `id` through its 3-strike/2-pass tracker; returns the status the
    /// UI should display.
    pub async fn probe_tracked(&mut self, id: &str) -> Result<HealthStatus, SupervisorError> {
        let raw = self.probe(id).await?;
        let tracker = self.trackers.entry(id.to_string()).or_default();
        Ok(tracker.observe(raw))
    }

    /// Status snapshot over all registered specs, sorted by id.
    pub fn status(&self) -> Vec<ProcessStatus> {
        let mut out: Vec<ProcessStatus> = self
            .specs
            .keys()
            .map(|id| {
                let restarts = self.crash_counts.get(id).copied().unwrap_or(0);
                if let Some(proc) = self.live.get(id) {
                    ProcessStatus {
                        id: id.clone(),
                        state: proc.state(),
                        pid: proc.pid(),
                        adopted: proc.adopted_pid.is_some(),
                        restarts,
                        uptime: proc.uptime(),
                    }
                } else if self.failed_ids.contains(id) {
                    ProcessStatus {
                        id: id.clone(),
                        state: ProcessState::Failed,
                        pid: None,
                        adopted: false,
                        restarts,
                        uptime: None,
                    }
                } else {
                    ProcessStatus {
                        id: id.clone(),
                        state: ProcessState::Stopped,
                        pid: None,
                        adopted: false,
                        restarts,
                        uptime: None,
                    }
                }
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Exact argv the supervisor would spawn for `id` (wizard "Show commands"
    /// and backend "launch command" UI must equal this).
    pub fn launch_argv(&self, id: &str) -> Result<Vec<String>, SupervisorError> {
        self.specs
            .get(id)
            .map(|s| s.argv())
            .ok_or_else(|| SupervisorError::NotFound(id.to_string()))
    }

    /// The current-generation ring for `id`: the live process's ring, or
    /// the retained ring of a stopped/reaped one.
    fn current_ring(&self, id: &str) -> Option<Arc<Mutex<RingBuffer<LogRecord>>>> {
        self.live
            .get(id)
            .map(|p| p.log_ring())
            .or_else(|| self.dead_rings.get(id).cloned())
    }

    /// Every retained log scope, sorted by id: live processes, retained
    /// rings of stopped ones, and ids with an archived previous run.
    /// Drives the Logs view's scope picker — including scopes whose
    /// process no longer exists in the configuration at all.
    pub fn log_scopes(&self) -> Vec<LogScope> {
        let mut ids: Vec<&String> = self
            .live
            .keys()
            .chain(self.dead_rings.keys())
            .chain(self.archived_rings.keys())
            .collect();
        ids.sort();
        ids.dedup();
        ids.into_iter()
            .map(|id| LogScope {
                id: id.clone(),
                previous: self.archived_rings.contains_key(id),
            })
            .collect()
    }

    /// Query captured logs across scopes (spec §14 `logs_query`).
    ///
    /// `scope` is a process id or `"all"`. `level` is an optional
    /// error/warn/info/debug filter; llama-server emits no structured
    /// levels, so this is a documented heuristic substring match on the
    /// line. `search` is an optional case-insensitive substring.
    ///
    /// A single scope covers the process's current ring plus its archived
    /// previous-run ring when one exists (archived records are flagged
    /// `archived`). Scope `"all"` merges **every** ring — live, retained,
    /// and archived — into one stream. Either way the result is sorted
    /// newest-first by timestamp and only then capped at `limit`, so a
    /// chatty process can no longer crowd every other process's newest
    /// lines out of an "all" page. [`LogsQueryResult::total_matching`]
    /// counts all matches before the cap, letting callers signal
    /// truncation. Every returned record is stamped with `source` (its
    /// process id).
    pub fn logs_query(
        &self,
        scope: &str,
        level: Option<&str>,
        search: Option<&str>,
        limit: usize,
    ) -> LogsQueryResult {
        let level_filter = level.and_then(LogLevelFilter::parse);
        let search_lc = search.map(|s| s.to_lowercase());

        // Resolve the rings in scope: (id, ring, archived) triples,
        // deterministic id order for deterministic tie-breaking below.
        let mut scope_ids: Vec<&String> = if scope == "all" {
            let mut all: Vec<&String> = self
                .live
                .keys()
                .chain(self.dead_rings.keys())
                .chain(self.archived_rings.keys())
                .collect();
            all.sort();
            all.dedup();
            all
        } else {
            self.live
                .keys()
                .chain(self.dead_rings.keys())
                .chain(self.archived_rings.keys())
                .find(|k| k.as_str() == scope)
                .into_iter()
                .collect()
        };
        scope_ids.sort();
        let mut rings: Vec<ScopedRing<'_>> = Vec::new();
        for id in scope_ids {
            if let Some(ring) = self.current_ring(id) {
                rings.push((id, ring, false));
            }
            if let Some(ring) = self.archived_rings.get(id) {
                rings.push((id, Arc::clone(ring), true));
            }
        }

        let mut matched: Vec<LogRecord> = Vec::new();
        for (id, ring, archived) in &rings {
            let Ok(guard) = ring.lock() else { continue };
            for rec in guard.recent(usize::MAX) {
                if let Some(lf) = level_filter {
                    if !lf.matches_line(&rec.line) {
                        continue;
                    }
                }
                if let Some(ref needle) = search_lc {
                    if !rec.line.to_lowercase().contains(needle) {
                        continue;
                    }
                }
                let mut rec = rec.clone();
                rec.source = (*id).clone();
                rec.archived = *archived;
                matched.push(rec);
            }
        }
        // Newest-first across the merged stream. The sort is stable and
        // each ring contributed newest-first, so records sharing a
        // timestamp keep their per-ring newest-first order.
        matched.sort_by_key(|r| std::cmp::Reverse(r.ts));
        let total_matching = matched.len();
        matched.truncate(limit.max(1));
        LogsQueryResult {
            records: matched,
            total_matching,
        }
    }
}

/// One ring participating in a [`Supervisor::logs_query`]: the owning
/// process id, the ring, and whether it is the archived previous run.
type ScopedRing<'a> = (&'a String, Arc<Mutex<RingBuffer<LogRecord>>>, bool);

/// Result of [`Supervisor::logs_query`]: the requested newest-first page
/// plus the total number of records that matched the filters before the
/// limit was applied, so callers can signal truncation instead of
/// rendering a capped slice as if it were complete.
#[derive(Debug, Clone)]
pub struct LogsQueryResult {
    pub records: Vec<LogRecord>,
    pub total_matching: usize,
}

/// One retained log scope, as enumerated by [`Supervisor::log_scopes`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogScope {
    pub id: String,
    /// True when the id also has an archived previous-run ring, i.e. its
    /// logs include records from before the most recent spawn, flagged
    /// `archived`. Querying the id returns current and archived lines
    /// merged (incl. previous run).
    pub previous: bool,
}

/// Log-level filter for [`Supervisor::logs_query`].
///
/// llama-server has no structured log levels; matching is a heuristic over
/// well-known tokens in the line. `Error` also matches stderr-origin lines
/// via "error"/"fail"/"panic"/"fatal"; `Warn` matches "warn"; `Info`
/// matches "info"; `Debug` matches "debug"/"trace".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevelFilter {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevelFilter {
    /// Parse a UI-supplied level string; unknown strings are `None`
    /// (caller treats as no filter).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "error" | "err" | "fatal" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" | "trace" => Some(Self::Debug),
            _ => None,
        }
    }

    /// Whether a raw log line passes this level filter (the same
    /// heuristic [`Supervisor::logs_query`] applies). Exposed so other
    /// log sources merged into the Logs view (the file-backed gateway
    /// log) are filtered identically.
    pub fn matches(&self, line: &str) -> bool {
        self.matches_line(line)
    }

    fn matches_line(&self, line: &str) -> bool {
        let lc = line.to_lowercase();
        match self {
            Self::Error => ["error", "fail", "panic", "fatal"]
                .iter()
                .any(|t| lc.contains(t)),
            Self::Warn => lc.contains("warn"),
            Self::Info => lc.contains("info"),
            Self::Debug => lc.contains("debug") || lc.contains("trace"),
        }
    }
}

/// Result of a backend test request (spec §14 `backend_test_request`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub status: u16,
    pub content: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub latency_ms: u64,
}

/// Minimal inference test request: HTTP POST to
/// `{base_url}/v1/chat/completions` with a tiny prompt, returning the first
/// choice's content plus usage when the server reports it.
///
/// `base_url` is e.g. `http://127.0.0.1:8081`. Non-2xx responses are not
/// errors here — the status is reported and content is `None` — but
/// transport failures and unparseable bodies are.
pub async fn test_request(base_url: &str, prompt: &str) -> Result<TestResult, SupervisorError> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": "test",
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 16,
        "stream": false,
    });
    // NOTE: built without reqwest's `json` feature (workspace keeps reqwest
    // minimal); body serialization and response parsing go through
    // serde_json directly.
    let body_str =
        serde_json::to_string(&body).map_err(|e| SupervisorError::Http(e.to_string()))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let start = Instant::now();
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    let latency_ms = start.elapsed().as_millis() as u64;
    let text = resp
        .text()
        .await
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| SupervisorError::Http(format!("unparseable response body: {e}")))?;
    let content = json
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let prompt_tokens = json
        .pointer("/usage/prompt_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let completion_tokens = json
        .pointer("/usage/completion_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    Ok(TestResult {
        status,
        content,
        prompt_tokens,
        completion_tokens,
        latency_ms,
    })
}

/// Name of the probe tool offered by [`test_tool_call`]. A reply
/// only verifies when it calls a tool with exactly this name.
pub const TOOL_CALL_PROBE_TOOL_NAME: &str = "probe_get_weather";

/// Outcome of a backend tool-call probe (MCP design §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallProbeOutcome {
    /// `choices[0].message.tool_calls` was present, named the probe
    /// tool, and its `arguments` parsed as JSON.
    Verified,
    /// No structured `tool_calls`, but tool-call markup leaked into
    /// the message content (e.g. `<tool_call>…`): the model tried to
    /// call the tool and the server/template did not structure it.
    Unparsed,
    /// Anything else: the health gate failed, the request failed, or
    /// the reply was plain content with no tool call at all.
    Failed,
}

/// Result of a backend tool-call probe ([`test_tool_call`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallProbeResult {
    pub outcome: ToolCallProbeOutcome,
    /// The completion's `finish_reason`, recorded either way;
    /// `None` when no completion was received.
    pub finish_reason: Option<String>,
    /// Evidence from `GET /props`: whether a `chat_template_tool_use`
    /// entry was present (at the top level or inside
    /// `chat_template_caps`). Absence is evidence, not failure.
    pub chat_template_tool_use: bool,
    /// The `chat_template_caps` summary from `GET /props`, when the
    /// server reported one; `None` when `/props` failed or carried
    /// no caps.
    pub chat_template_caps: Option<serde_json::Value>,
    /// Human-readable summary of what the probe observed.
    pub detail: String,
    pub latency_ms: u64,
}

impl ToolCallProbeResult {
    fn failed(
        detail: impl Into<String>,
        chat_template_tool_use: bool,
        chat_template_caps: Option<serde_json::Value>,
        finish_reason: Option<String>,
        latency_ms: u64,
    ) -> Self {
        Self {
            outcome: ToolCallProbeOutcome::Failed,
            finish_reason,
            chat_template_tool_use,
            chat_template_caps,
            detail: detail.into(),
            latency_ms,
        }
    }
}

/// Tool-call probe for one backend (MCP design §7.2): verifies the
/// backend actually emits structured `tool_calls`, which MCP tools
/// reaching the model depend on. Built beside [`test_request`] on
/// the same transport skeleton, but a new function — `test_request`
/// is hard-coded at `max_tokens: 16` and parses only `content`.
///
/// Steps: (1) `GET {base_url}/health` gates the probe — a failure
/// ends it as [`ToolCallProbeOutcome::Failed`]. (2) `GET /props`
/// records the template evidence (see
/// [`ToolCallProbeResult::chat_template_tool_use`]); its absence is
/// not a failure. (3) A direct `POST /v1/chat/completions` to the
/// backend (loopback, bypassing the gateway, so llama.cpp behaviour
/// is isolated) with one probe tool that has a defined
/// `parameters.properties` entry (a tool with no properties trips a
/// known chat-template bug class), `tool_choice: "required"`, a
/// prompt that unambiguously needs the tool,
/// `chat_template_kwargs {enable_thinking: false}`, `max_tokens:
/// 256`, and a 30 s timeout.
///
/// Probe-level failures (health, transport, HTTP, unparseable
/// bodies, plain-content replies) are reported in the result as
/// `Failed`, not as `Err` — the result is the probe's answer.
pub async fn test_tool_call(base_url: &str) -> Result<ToolCallProbeResult, SupervisorError> {
    let base = base_url.trim_end_matches('/');
    let start = Instant::now();
    let elapsed = |start: &Instant| start.elapsed().as_millis() as u64;
    // NOTE: built without reqwest's `json` feature, like
    // test_request; bodies go through serde_json directly.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| SupervisorError::Http(e.to_string()))?;

    // (1) Health gate.
    match client.get(format!("{base}/health")).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            return Ok(ToolCallProbeResult::failed(
                format!(
                    "backend health check failed: GET /health returned HTTP {}",
                    resp.status().as_u16()
                ),
                false,
                None,
                None,
                elapsed(&start),
            ));
        }
        Err(e) => {
            return Ok(ToolCallProbeResult::failed(
                format!("backend health check failed: GET /health: {e}"),
                false,
                None,
                None,
                elapsed(&start),
            ));
        }
    }

    // (2) /props evidence.
    let mut chat_template_tool_use = false;
    let mut chat_template_caps: Option<serde_json::Value> = None;
    if let Ok(resp) = client.get(format!("{base}/props")).send().await {
        if resp.status().is_success() {
            if let Ok(text) = resp.text().await {
                if let Ok(props) = serde_json::from_str::<serde_json::Value>(&text) {
                    chat_template_caps = props.get("chat_template_caps").cloned();
                    chat_template_tool_use = props.get("chat_template_tool_use").is_some()
                        || chat_template_caps
                            .as_ref()
                            .and_then(|caps| caps.as_object())
                            .map(|caps| caps.contains_key("chat_template_tool_use"))
                            .unwrap_or(false);
                }
            }
        }
    }

    // (3) The tool-call request itself.
    let body = serde_json::json!({
        "model": "test",
        "messages": [{
            "role": "user",
            "content": "What is the weather in Paris right now? You must use the probe_get_weather tool to find out.",
        }],
        "tools": [{
            "type": "function",
            "function": {
                "name": TOOL_CALL_PROBE_TOOL_NAME,
                "description": "Get the current weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {
                            "type": "string",
                            "description": "The city to get the weather for.",
                        },
                    },
                    "required": ["city"],
                },
            },
        }],
        "tool_choice": "required",
        "chat_template_kwargs": {"enable_thinking": false},
        "max_tokens": 256,
        "stream": false,
    });
    let body_str =
        serde_json::to_string(&body).map_err(|e| SupervisorError::Http(e.to_string()))?;
    let resp = match client
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return Ok(ToolCallProbeResult::failed(
                format!("tool-call request failed: {e}"),
                chat_template_tool_use,
                chat_template_caps,
                None,
                elapsed(&start),
            ));
        }
    };
    let status = resp.status().as_u16();
    let text = match resp.text().await {
        Ok(text) => text,
        Err(e) => {
            return Ok(ToolCallProbeResult::failed(
                format!("tool-call response could not be read: {e}"),
                chat_template_tool_use,
                chat_template_caps,
                None,
                elapsed(&start),
            ));
        }
    };
    if !(200..300).contains(&status) {
        return Ok(ToolCallProbeResult::failed(
            format!("tool-call request returned HTTP {status}"),
            chat_template_tool_use,
            chat_template_caps,
            None,
            elapsed(&start),
        ));
    }
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(e) => {
            return Ok(ToolCallProbeResult::failed(
                format!("tool-call response was not valid JSON: {e}"),
                chat_template_tool_use,
                chat_template_caps,
                None,
                elapsed(&start),
            ));
        }
    };
    let finish_reason = json
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let message = json.pointer("/choices/0/message");
    let tool_calls = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|v| v.as_array());
    if let Some(calls) = tool_calls {
        if let Some(call) = calls.first() {
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name != TOOL_CALL_PROBE_TOOL_NAME {
                return Ok(ToolCallProbeResult::failed(
                    format!(
                        "backend called tool {name:?}, not the probe tool {TOOL_CALL_PROBE_TOOL_NAME:?}"
                    ),
                    chat_template_tool_use,
                    chat_template_caps,
                    finish_reason,
                    elapsed(&start),
                ));
            }
            // Arguments arrive as a JSON string (OpenAI shape) or,
            // from some servers, as an already-parsed object; either
            // form must be valid JSON to verify.
            let arguments_ok = match call.pointer("/function/arguments") {
                Some(serde_json::Value::String(s)) => {
                    serde_json::from_str::<serde_json::Value>(s).is_ok()
                }
                Some(serde_json::Value::Object(_)) | Some(serde_json::Value::Array(_)) => true,
                _ => false,
            };
            if arguments_ok {
                return Ok(ToolCallProbeResult {
                    outcome: ToolCallProbeOutcome::Verified,
                    finish_reason,
                    chat_template_tool_use,
                    chat_template_caps,
                    detail: format!(
                        "backend emitted a structured tool call to {TOOL_CALL_PROBE_TOOL_NAME} with JSON arguments"
                    ),
                    latency_ms: elapsed(&start),
                });
            }
            return Ok(ToolCallProbeResult::failed(
                "backend emitted a tool call whose arguments did not parse as JSON",
                chat_template_tool_use,
                chat_template_caps,
                finish_reason,
                elapsed(&start),
            ));
        }
    }
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if content.contains("<tool_call") || content.contains("<|tool_call") {
        return Ok(ToolCallProbeResult {
            outcome: ToolCallProbeOutcome::Unparsed,
            finish_reason,
            chat_template_tool_use,
            chat_template_caps,
            detail:
                "tool-call markup leaked into the message content instead of structured tool_calls"
                    .to_string(),
            latency_ms: elapsed(&start),
        });
    }
    Ok(ToolCallProbeResult::failed(
        "backend replied with plain content and no tool call",
        chat_template_tool_use,
        chat_template_caps,
        finish_reason,
        elapsed(&start),
    ))
}

/// One chat message for [`chat_completion`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Result of a full chat completion (the Optimize consultation's
/// transport — design docs/optimize-design.md §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResult {
    pub content: String,
    pub finish_reason: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub latency_ms: u64,
    /// The HTTP status the backend answered with (always 2xx —
    /// anything else is an error before this is built).
    pub http_status: u16,
    /// Transport size: the response body's byte length, before
    /// any extraction. Kept beside `content` so a caller can
    /// distinguish "the backend sent nothing" from "the backend
    /// sent a body whose answer text extracted to nothing".
    pub response_bytes: usize,
    /// Size of the first choice's `reasoning_content`, when the
    /// model returned one: a thinking model that spends its whole
    /// token budget reasoning leaves the answer in this field's
    /// shadow — `content` empty, this non-zero.
    pub reasoning_bytes: usize,
}

/// The message text of a chat-completion choice: a plain string,
/// or an array of content parts (the multimodal shape some
/// servers emit even for text-only answers) whose text parts are
/// joined. Anything else extracts to an empty string.
fn message_text(message: &serde_json::Value, field: &str) -> String {
    match message.get(field) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect(),
        _ => String::new(),
    }
}

/// Interpret a chat-completion response body (the pure core of
/// [`chat_completion`], split out for tests): parse the JSON,
/// extract the first choice's answer text, and — critically —
/// refuse an empty answer. Before Oct 19, 2026 an empty `content`
/// string was returned as a successful empty completion, which
/// the Optimize consultation then logged as a "0 bytes" response
/// and failed to parse as a plan, with the finish reason and
/// usage that would have explained it discarded on the floor.
/// An empty answer is now an error that names the finish reason,
/// the token counts, and how much reasoning text the model
/// produced instead, so the failure is diagnosable from the
/// error alone.
fn interpret_chat_completion(
    http_status: u16,
    text: &str,
    latency_ms: u64,
    max_tokens: u32,
) -> Result<ChatCompletionResult, SupervisorError> {
    let json: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| SupervisorError::Http(format!("unparseable response body: {e}")))?;
    let message = json
        .pointer("/choices/0/message")
        .ok_or_else(|| {
            SupervisorError::Http("response has no first-choice message".to_string())
        })?;
    let content = message_text(message, "content");
    let reasoning = message_text(message, "reasoning_content");
    let finish_reason = json
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let prompt_tokens = json
        .pointer("/usage/prompt_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let completion_tokens = json
        .pointer("/usage/completion_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    if content.trim().is_empty() {
        let usage = format!(
            "finish_reason {}, completion_tokens {}",
            finish_reason.as_deref().unwrap_or("absent"),
            completion_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "absent".to_string()),
        );
        return Err(SupervisorError::Http(if reasoning.is_empty() {
            format!(
                "backend returned an empty completion ({usage}; response body {} bytes) — no answer text was produced",
                text.len(),
            )
        } else if finish_reason.as_deref() == Some("length") {
            format!(
                "backend returned no answer text: the model spent the full {max_tokens}-token budget on reasoning ({} reasoning bytes, {usage}) — no content was produced",
                reasoning.len(),
            )
        } else {
            format!(
                "backend returned no answer text, only reasoning ({} bytes, {usage})",
                reasoning.len(),
            )
        }));
    }
    Ok(ChatCompletionResult {
        content,
        finish_reason,
        prompt_tokens,
        completion_tokens,
        latency_ms,
        http_status,
        response_bytes: text.len(),
        reasoning_bytes: reasoning.len(),
    })
}

/// A non-streaming chat completion against a backend's OpenAI
/// surface: `POST {base_url}/v1/chat/completions` with caller-set
/// temperature, max_tokens, and timeout.
///
/// This is deliberately NOT an extension of [`test_request`]: the
/// test request is a probe (30 s, 16 tokens, status reported rather
/// than errored), while a consultation is a minutes-long generation
/// with its own budget where a non-2xx status or a missing first
/// choice is a failure. Cancellation is by future drop: dropping
/// this future aborts the in-flight HTTP request, which is what
/// the Optimize scoped-cancellation handle relies on.
pub async fn chat_completion(
    base_url: &str,
    model: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    timeout: Duration,
) -> Result<ChatCompletionResult, SupervisorError> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": false,
        // The consultation wants a structured answer, not a
        // reasoning trace: a thinking model left to itself can
        // spend the entire token budget on reasoning and return
        // an empty `content` (observed on Pete's Qwen3 planners,
        // Oct 19, 2026 — 4,096 reasoning tokens at ~20 tok/s is
        // the ~200 s the failed consultations took). Templates
        // that do not know the kwarg ignore it.
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let body_str =
        serde_json::to_string(&body).map_err(|e| SupervisorError::Http(e.to_string()))?;
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let start = Instant::now();
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let latency_ms = start.elapsed().as_millis() as u64;
    if !(200..300).contains(&status) {
        let excerpt: String = text.chars().take(300).collect();
        return Err(SupervisorError::Http(format!(
            "backend returned HTTP {status}: {excerpt}"
        )));
    }
    interpret_chat_completion(status, &text, latency_ms, max_tokens)
}

/// Result of one benchmark completion (Optimize Phase 5a,
/// design addendum A.1): the generation figures come from
/// llama.cpp's own `timings` object, never from wall-clock
/// token counting, because the server is the authority on how
/// fast it processed and predicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSample {
    /// Time from request start to the first streamed chunk
    /// carrying non-empty content. `None` when the response was
    /// not an event stream (the non-streaming fallback) — never
    /// estimated (design A.1).
    pub ttft_ms: Option<f64>,
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    pub content: String,
    pub latency_ms: u64,
}

/// Rates from a llama.cpp `timings` object: the direct
/// per-second fields when present, else derived from the
/// millisecond / token counts the same object carries
/// (`prompt_ms` + `prompt_n`, `predicted_ms` + `predicted_n`).
fn timings_rates(json: &serde_json::Value) -> (Option<f64>, Option<f64>) {
    let Some(t) = json.get("timings") else {
        return (None, None);
    };
    let rate = |per_second: &str, ms: &str, n: &str| -> Option<f64> {
        if let Some(v) = t.get(per_second).and_then(|v| v.as_f64()) {
            if v > 0.0 {
                return Some(v);
            }
        }
        let ms = t.get(ms).and_then(|v| v.as_f64())?;
        let n = t.get(n).and_then(|v| v.as_f64())?;
        if ms > 0.0 && n > 0.0 {
            Some(n / (ms / 1000.0))
        } else {
            None
        }
    };
    (
        rate("prompt_per_second", "prompt_ms", "prompt_n"),
        rate("predicted_per_second", "predicted_ms", "predicted_n"),
    )
}

fn absorb_timings(
    json: &serde_json::Value,
    prompt_per_second: &mut Option<f64>,
    predicted_per_second: &mut Option<f64>,
) {
    let (p, g) = timings_rates(json);
    if p.is_some() {
        *prompt_per_second = p;
    }
    if g.is_some() {
        *predicted_per_second = g;
    }
}

/// The server's reported build string (`GET /props` ->
/// `build_info`), best-effort: any failure is `None`, because
/// the build is provenance metadata — a measurement records it
/// when obtainable (predicted-throughput design §4.2) — never
/// a condition of the operation that asked.
pub async fn server_build_info(base_url: &str) -> Option<String> {
    let url = format!("{}/props", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    let props: serde_json::Value = serde_json::from_str(&text).ok()?;
    props
        .get("build_info")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A streaming benchmark completion against a backend's OpenAI
/// surface: `POST {base_url}/v1/chat/completions` with
/// `stream: true` and caller-set temperature / max_tokens.
///
/// Reads the SSE stream as it arrives: the time to the first
/// chunk carrying non-empty delta content is the TTFT, and
/// llama.cpp's `timings` object is taken from whichever chunk
/// carries it (llama.cpp puts it on the final chunk). When the
/// server answers with a plain JSON completion instead of an
/// event stream, that body is parsed the same way and TTFT
/// stays `None`. Cancellation is by future drop, exactly like
/// [`chat_completion`] — the Optimize job slot relies on it.
pub async fn benchmark_completion(
    base_url: &str,
    model: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    timeout: Duration,
) -> Result<BenchmarkSample, SupervisorError> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": true,
    });
    let body_str =
        serde_json::to_string(&body).map_err(|e| SupervisorError::Http(e.to_string()))?;
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let start = Instant::now();
    let mut resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        let excerpt: String = text.chars().take(300).collect();
        return Err(SupervisorError::Http(format!(
            "backend returned HTTP {status}: {excerpt}"
        )));
    }
    let is_event_stream = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if !is_event_stream {
        // Non-streaming fallback: a plain JSON completion with
        // timings. TTFT is not measurable here and stays None.
        let text = resp
            .text()
            .await
            .map_err(|e| SupervisorError::Http(e.to_string()))?;
        let latency_ms = start.elapsed().as_millis() as u64;
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| SupervisorError::Http(format!("unparseable response body: {e}")))?;
        let mut prompt_per_second = None;
        let mut predicted_per_second = None;
        absorb_timings(&json, &mut prompt_per_second, &mut predicted_per_second);
        let content = json
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        return Ok(BenchmarkSample {
            ttft_ms: None,
            prompt_per_second,
            predicted_per_second,
            content,
            latency_ms,
        });
    }

    let mut content = String::new();
    let mut ttft_ms: Option<f64> = None;
    let mut prompt_per_second = None;
    let mut predicted_per_second = None;
    let mut buffer = String::new();
    // Process one SSE line: `data:` payloads carry the chunk
    // JSON; `[DONE]` and blank separators carry nothing.
    macro_rules! process_line {
        ($line:expr) => {
            if let Some(data) = $line.trim().strip_prefix("data:") {
                let data = data.trim();
                if !data.is_empty() && data != "[DONE]" {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        absorb_timings(&json, &mut prompt_per_second, &mut predicted_per_second);
                        if let Some(delta) = json
                            .pointer("/choices/0/delta/content")
                            .and_then(|v| v.as_str())
                        {
                            if !delta.is_empty() {
                                if ttft_ms.is_none() {
                                    ttft_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
                                }
                                content.push_str(delta);
                            }
                        }
                    }
                }
            }
        };
    }
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=pos).collect();
                    process_line!(line);
                }
            }
            Ok(None) => break,
            Err(e) => return Err(SupervisorError::Http(e.to_string())),
        }
    }
    if !buffer.is_empty() {
        process_line!(buffer);
    }
    Ok(BenchmarkSample {
        ttft_ms,
        prompt_per_second,
        predicted_per_second,
        content,
        latency_ms: start.elapsed().as_millis() as u64,
    })
}

/// Supervisor errors.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("spawn failed for {0}: {1}")]
    SpawnFailed(String, String),
    #[error("process not found: {0}")]
    NotFound(String),
    #[error("process already running: {0}")]
    AlreadyRunning(String),
    #[error("port {0} is already in use")]
    PortInUse(u16),
    #[error("drain timed out for {0}")]
    DrainTimeout(String),
    #[error("health probe failed for {0}: {1}")]
    ProbeFailed(String, String),
    #[error("restart budget exhausted for {0}")]
    RestartBudgetExhausted(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the cfg(unix) port-occupancy test binds a std listener.
    #[cfg(unix)]
    use std::net::TcpListener as StdTcpListener;

    #[test]
    fn unregister_spec_removes_idle_and_reports() {
        let mut sup = Supervisor::new(test_policy());
        let spec = ProcessSpec {
            id: "idle".into(),
            program: PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            workdir: None,
            port: None,
        };
        sup.register_spec(spec);
        assert!(sup.spec("idle").is_some());
        assert!(sup.unregister_spec("idle"));
        assert!(sup.spec("idle").is_none());
        // Second removal and unknown ids report false.
        assert!(!sup.unregister_spec("idle"));
        assert!(!sup.unregister_spec("never-registered"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unregister_spec_refused_while_live() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec("sleeper", "exec sleep 30"));
        sup.spawn_backend("sleeper").await.unwrap();
        assert!(!sup.unregister_spec("sleeper"));
        assert!(sup.spec("sleeper").is_some());
        sup.stop("sleeper", false).await.unwrap();
        assert!(sup.unregister_spec("sleeper"));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn test_policy() -> BackoffPolicy {
        BackoffPolicy {
            base: ms(100),
            max: Duration::from_secs(5),
            max_retries: 3,
        }
    }

    // Only the cfg(unix) spawn/log tests use these three helpers;
    // gating them the same way keeps the Windows test target free of
    // dead code.
    #[cfg(unix)]
    fn test_ring() -> Arc<Mutex<RingBuffer<LogRecord>>> {
        Arc::new(Mutex::new(RingBuffer::new(1024)))
    }

    /// NOTE: `exec` replaces the shell so signals hit the real child and no
    /// orphaned grandchildren survive the test.
    #[cfg(unix)]
    fn sh_spec(id: &str, script: &str) -> ProcessSpec {
        ProcessSpec {
            id: id.into(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            workdir: None,
            port: None,
        }
    }

    /// Registration spec for the adoption tests, on either platform.
    /// Adoption attaches to an already-running pid and never spawns
    /// the spec's program; the spec names the same long-lived sleeper
    /// the tests launch externally (`sleep 60` via sh on Unix, `ping`
    /// on Windows) so the registration describes the adopted process.
    fn ext_spec(id: &str) -> ProcessSpec {
        #[cfg(unix)]
        {
            sh_spec(id, "exec sleep 60")
        }
        #[cfg(windows)]
        {
            ProcessSpec {
                id: id.into(),
                program: PathBuf::from("ping"),
                args: vec!["-n".into(), "60".into(), "127.0.0.1".into()],
                env: vec![],
                workdir: None,
                port: None,
            }
        }
    }

    #[cfg(unix)]
    async fn wait_for<F>(mut cond: F, timeout: Duration)
    where
        F: FnMut() -> bool,
    {
        let start = Instant::now();
        loop {
            if cond() {
                return;
            }
            assert!(
                start.elapsed() < timeout,
                "condition not met within {timeout:?}"
            );
            tokio::time::sleep(ms(20)).await;
        }
    }

    #[cfg(unix)]
    fn ring_lines_chronological(ring: &Arc<Mutex<RingBuffer<LogRecord>>>) -> Vec<String> {
        ring.lock()
            .unwrap()
            .recent(usize::MAX)
            .into_iter()
            .rev()
            .map(|r| r.line.clone())
            .collect()
    }

    async fn serve_once(status_code: u16) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let body = "ok";
                let resp = format!(
                    "HTTP/1.1 {status_code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
            }
        });
        port
    }

    struct StubProbe(HealthStatus);

    #[async_trait]
    impl HealthProbe for StubProbe {
        async fn probe(&self) -> Result<HealthStatus, SupervisorError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn backoff_delay_sequence_caps_and_allows_retry() {
        let p = test_policy();
        assert_eq!(p.delay_for_attempt(0), ms(100));
        assert_eq!(p.delay_for_attempt(1), ms(200));
        assert_eq!(p.delay_for_attempt(2), ms(400));
        // Capped at max, and the shift saturates (no overflow on huge attempts).
        assert_eq!(p.delay_for_attempt(10), Duration::from_secs(5));
        assert_eq!(p.delay_for_attempt(u32::MAX), Duration::from_secs(5));
        assert!(p.allows_retry(0));
        assert!(p.allows_retry(2));
        assert!(!p.allows_retry(3));
    }

    #[tokio::test]
    async fn adopt_external_process_lifecycle() {
        // A live process the supervisor did not spawn.
        #[cfg(unix)]
        let mut ext = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let mut ext = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = ext.id();
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(ext_spec("ext"));
        // Unknown id and double-adoption are errors, like spawn.
        assert!(sup.adopt("missing", pid).is_err());
        sup.adopt("ext", pid).unwrap();
        assert!(sup.adopt("ext", pid).is_err());
        let st = sup.status().into_iter().find(|s| s.id == "ext").unwrap();
        assert_eq!(st.state, ProcessState::Running);
        assert_eq!(st.pid, Some(pid));
        assert!(st.adopted);
        // Stop kills the external process by pid.
        sup.stop("ext", false).await.unwrap();
        let _ = ext.wait();
        assert!(!pid_alive(pid));
        let st = sup.status().into_iter().find(|s| s.id == "ext").unwrap();
        assert_eq!(st.state, ProcessState::Stopped);
    }

    #[tokio::test]
    async fn adopted_process_death_is_reaped_as_failed() {
        #[cfg(unix)]
        let mut ext = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let mut ext = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = ext.id();
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(ext_spec("ext"));
        sup.adopt("ext", pid).unwrap();
        // The external process dies on its own.
        ext.kill().unwrap();
        let _ = ext.wait();
        sup.reap_exited();
        let st = sup.status().into_iter().find(|s| s.id == "ext").unwrap();
        assert_eq!(st.state, ProcessState::Failed);
        assert_eq!(st.pid, None);
    }

    #[test]
    fn ring_buffer_overwrites_oldest_newest_first() {
        let mut ring = RingBuffer::new(3);
        for i in 0..5 {
            ring.push(i);
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.capacity(), 3);
        let recent: Vec<i32> = ring.recent(10).into_iter().copied().collect();
        assert_eq!(recent, vec![4, 3, 2]);
        // Zero capacity is clamped to 1 rather than panicking.
        let mut z = RingBuffer::new(0);
        z.push(1);
        assert_eq!(z.len(), 1);
    }

    #[test]
    fn log_record_carries_stream_and_timestamp() {
        let r = LogRecord::new(LogStream::Stderr, "boom");
        assert_eq!(r.stream, LogStream::Stderr);
        assert_eq!(r.line, "boom");
        assert!(r.ts <= Utc::now());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_captures_stdout_and_stderr() {
        let ring = test_ring();
        let mut proc = SupervisedProcess::spawn(
            "echo-test",
            &PathBuf::from("/bin/sh"),
            &[
                "-c".to_string(),
                "echo out-line; echo err-line >&2".to_string(),
            ],
            &[],
            ring.clone(),
        )
        .await
        .unwrap();

        wait_for(|| ring.lock().unwrap().len() >= 2, Duration::from_secs(5)).await;
        let lines = ring_lines_chronological(&ring);
        assert_eq!(lines, vec!["out-line".to_string(), "err-line".to_string()]);

        let streams: Vec<LogStream> = ring
            .lock()
            .unwrap()
            .recent(10)
            .into_iter()
            .map(|r| r.stream)
            .collect();
        assert!(streams.contains(&LogStream::Stdout));
        assert!(streams.contains(&LogStream::Stderr));

        // Already exited; kill/drain must still be Ok and idempotent.
        proc.kill().await.unwrap();
        assert!(!proc.is_running());
        proc.drain(ms(100)).await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_with_workdir_sets_cwd() {
        let ring = test_ring();
        let mut proc = SupervisedProcess::spawn_with_workdir(
            "pwd-test",
            Path::new("/bin/sh"),
            &["-c".to_string(), "pwd".to_string()],
            &[],
            Some(Path::new("/tmp")),
            ring.clone(),
        )
        .await
        .unwrap();
        wait_for(|| !ring.lock().unwrap().is_empty(), Duration::from_secs(5)).await;
        let lines = ring_lines_chronological(&ring);
        assert!(lines.iter().any(|l| l == "/tmp"), "lines: {lines:?}");
        proc.kill().await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_failure_reports_program() {
        let err = SupervisedProcess::spawn(
            "missing",
            &PathBuf::from("/nonexistent/binary-xyz"),
            &[],
            &[],
            test_ring(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SupervisorError::SpawnFailed(id, _) if id == "missing"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn drain_graceful_terminates_quickly() {
        let ring = test_ring();
        let mut proc = SupervisedProcess::spawn(
            "sleepy",
            &PathBuf::from("/bin/sh"),
            &["-c".to_string(), "exec sleep 30".to_string()],
            &[],
            ring,
        )
        .await
        .unwrap();
        assert!(proc.is_running());
        assert!(proc.pid().is_some());

        let start = Instant::now();
        proc.drain(Duration::from_secs(5)).await.unwrap();
        // SIGTERM kills `sleep` immediately: well under the 5 s budget.
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!proc.is_running());
        assert_eq!(proc.state(), ProcessState::Stopped);
        assert!(proc.uptime().is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn drain_timeout_kills_term_ignoring_process() {
        let ring = test_ring();
        let mut proc = SupervisedProcess::spawn(
            "stubborn",
            &PathBuf::from("/bin/sh"),
            &[
                "-c".to_string(),
                // `ready` is printed AFTER the trap is installed, so SIGTERM
                // is guaranteed ignored from here on (the disposition
                // survives `exec` per POSIX). Without this handshake the
                // test races the child's startup.
                "trap '' TERM; echo ready; exec sleep 30".to_string(),
            ],
            &[],
            ring.clone(),
        )
        .await
        .unwrap();

        wait_for(
            || ring_lines_chronological(&ring).iter().any(|l| l == "ready"),
            Duration::from_secs(5),
        )
        .await;

        // Premise check: the child must be alive AND actually ignoring
        // SIGTERM before we measure drain's timing. `trap '' TERM` is
        // installed before `echo ready`, and SIG_IGN survives `exec` per
        // POSIX, so a live child at this point is provably TERM-ignoring.
        // If the child is already gone (e.g. a sandbox that blocks
        // `exec sleep`), the scenario cannot run here: fail with the
        // premise spelled out, never as a confusing "drain returned too
        // fast" timing flake.
        let pid = proc
            .pid()
            .expect("child must be alive after the ready handshake");
        let raw = nix::unistd::Pid::from_raw(pid as i32);
        nix::sys::signal::kill(raw, nix::sys::signal::Signal::SIGTERM)
            .expect("premise probe: SIGTERM delivery to the child must succeed");
        tokio::time::sleep(ms(150)).await;
        assert!(
            nix::sys::signal::kill(raw, None).is_ok(),
            "premise broken: TERM-ignoring child (pid {pid}) is gone before drain; \
             this environment cannot exec `sleep` or does not honor the trap, so the \
             drain-timeout scenario is unsupported here"
        );

        let start = Instant::now();
        proc.drain(ms(300)).await.unwrap();
        let elapsed = start.elapsed();
        // The full graceful window must elapse before the kill path is taken.
        assert!(elapsed >= ms(250), "drain returned too fast: {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "drain took too long: {elapsed:?}"
        );
        assert!(!proc.is_running());

        // The kill path is observable in the log ring.
        wait_for(
            || {
                ring_lines_chronological(&ring)
                    .iter()
                    .any(|l| l.contains("drain timed out"))
            },
            Duration::from_secs(2),
        )
        .await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn kill_terminates_and_reaps() {
        let mut proc = SupervisedProcess::spawn(
            "killme",
            &PathBuf::from("/bin/sh"),
            &["-c".to_string(), "exec sleep 30".to_string()],
            &[],
            test_ring(),
        )
        .await
        .unwrap();
        proc.kill().await.unwrap();
        assert!(!proc.is_running());
        assert_eq!(proc.state(), ProcessState::Stopped);
        assert!(proc.last_exit().is_some());
        // Second kill is a no-op.
        proc.kill().await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn process_crash_budget_counts_and_resets() {
        let policy = test_policy();
        let mut proc = SupervisedProcess::spawn(
            "budget",
            &PathBuf::from("/bin/sh"),
            &["-c".to_string(), "exec sleep 30".to_string()],
            &[],
            test_ring(),
        )
        .await
        .unwrap();

        assert_eq!(proc.note_crash(&policy).unwrap(), ms(100));
        assert_eq!(proc.note_crash(&policy).unwrap(), ms(200));
        assert_eq!(proc.restart_attempts(), 2);
        proc.note_healthy();
        assert_eq!(proc.restart_attempts(), 0);
        assert_eq!(proc.note_crash(&policy).unwrap(), ms(100));

        // Exhaust the budget: 3 allowed, 4th fails.
        proc.note_healthy();
        for _ in 0..3 {
            proc.note_crash(&policy).unwrap();
        }
        assert!(matches!(
            proc.note_crash(&policy),
            Err(SupervisorError::RestartBudgetExhausted(_))
        ));
        proc.kill().await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn supervisor_lifecycle_status_and_restart() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec("sleeper", "exec sleep 30"));

        // Unknown id / unknown launch argv.
        assert!(matches!(
            sup.spawn_backend("ghost").await,
            Err(SupervisorError::NotFound(_))
        ));
        assert!(matches!(
            sup.launch_argv("ghost"),
            Err(SupervisorError::NotFound(_))
        ));

        sup.spawn_backend("sleeper").await.unwrap();
        let st = sup.status();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].id, "sleeper");
        assert_eq!(st[0].state, ProcessState::Running);
        assert!(st[0].pid.is_some());
        assert!(st[0].uptime.is_some());
        assert_eq!(st[0].restarts, 0);

        // Double-spawn is rejected.
        assert!(matches!(
            sup.spawn_backend("sleeper").await,
            Err(SupervisorError::AlreadyRunning(_))
        ));

        sup.restart("sleeper", false).await.unwrap();
        assert_eq!(sup.status()[0].state, ProcessState::Running);

        sup.stop("sleeper", false).await.unwrap();
        let st = sup.status();
        assert_eq!(st[0].state, ProcessState::Stopped);
        assert!(st[0].pid.is_none());

        // Stopping an unknown id is NotFound.
        assert!(matches!(
            sup.stop("ghost", false).await,
            Err(SupervisorError::NotFound(_))
        ));
        // Restart of an unknown id is a fresh-start attempt -> NotFound (no spec).
        assert!(matches!(
            sup.restart("ghost", false).await,
            Err(SupervisorError::NotFound(_))
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn launch_argv_matches_spawned_command() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(ProcessSpec {
            id: "llama".into(),
            program: PathBuf::from("/opt/llama/llama-server"),
            args: vec![
                "--port".into(),
                "8081".into(),
                "--tensor-split".into(),
                "0.6,0.4".into(),
            ],
            env: vec![("CUDA_VISIBLE_DEVICES".into(), "0,1".into())],
            workdir: None,
            port: Some(8081),
        });
        assert_eq!(
            sup.launch_argv("llama").unwrap(),
            vec![
                "/opt/llama/llama-server",
                "--port",
                "8081",
                "--tensor-split",
                "0.6,0.4"
            ]
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_rejected_when_port_occupied() {
        // Hold a port open for the duration of the test.
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut sup = Supervisor::new(test_policy());
        let mut spec = sh_spec("web", "exec sleep 30");
        spec.port = Some(port);
        sup.register_spec(spec);

        let err = sup.spawn_backend("web").await.unwrap_err();
        assert!(matches!(err, SupervisorError::PortInUse(p) if p == port));
        // Nothing was spawned.
        assert_eq!(sup.status()[0].state, ProcessState::Stopped);
        drop(listener);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn reap_exited_marks_failed_and_crash_delay_applies() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec("quitter", "exit 3"));
        sup.spawn_backend("quitter").await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let dead = loop {
            let d = sup.reap_exited();
            if !d.is_empty() {
                break d;
            }
            assert!(Instant::now() < deadline, "child never exited");
            tokio::time::sleep(ms(20)).await;
        };
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].0, "quitter");
        assert!(!dead[0].1.success());

        assert_eq!(sup.status()[0].state, ProcessState::Failed);

        // Backoff sequence via the supervisor-level budget.
        assert_eq!(sup.crash_delay("quitter").unwrap(), ms(100));
        assert_eq!(sup.crash_delay("quitter").unwrap(), ms(200));
        assert_eq!(sup.crash_delay("quitter").unwrap(), ms(400));
        assert!(matches!(
            sup.crash_delay("quitter"),
            Err(SupervisorError::RestartBudgetExhausted(_))
        ));
        sup.note_healthy("quitter");
        assert_eq!(sup.crash_delay("quitter").unwrap(), ms(100));
        assert!(matches!(
            sup.crash_delay("ghost"),
            Err(SupervisorError::NotFound(_))
        ));

        // Unregistering the reaped id clears its runtime history:
        // status() no longer reports it, and crash_delay treats it as
        // unknown rather than resurrecting the old budget.
        assert!(sup.unregister_spec("quitter"));
        assert!(sup.status().iter().all(|s| s.id != "quitter"));
        assert!(matches!(
            sup.crash_delay("quitter"),
            Err(SupervisorError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn http_probe_maps_status_codes() {
        let port = serve_once(200).await;
        let p = HttpHealthProbe::new(
            format!("http://127.0.0.1:{port}/health"),
            Duration::from_secs(2),
        );
        assert_eq!(p.probe().await.unwrap(), HealthStatus::Healthy);

        let port = serve_once(500).await;
        let p = HttpHealthProbe::new(
            format!("http://127.0.0.1:{port}/health"),
            Duration::from_secs(2),
        );
        assert!(matches!(
            p.probe().await.unwrap(),
            HealthStatus::Degraded(_)
        ));

        // Nothing listening: connection refused -> Unhealthy, not an error.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let p = HttpHealthProbe::new(
            format!("http://127.0.0.1:{port}/health"),
            Duration::from_secs(2),
        );
        assert!(matches!(
            p.probe().await.unwrap(),
            HealthStatus::Unhealthy(_)
        ));
    }

    #[tokio::test]
    async fn http_probe_times_out() {
        // Accept but never respond: the client timeout must fire.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let p = HttpHealthProbe::new(format!("http://127.0.0.1:{port}/health"), ms(200));
        assert!(matches!(
            p.probe().await.unwrap(),
            HealthStatus::Unhealthy(msg) if msg.contains("timed out")
        ));
    }

    #[test]
    fn health_tracker_three_strikes_two_passes() {
        let mut t = HealthTracker::new();
        assert_eq!(t.observe(HealthStatus::Healthy), HealthStatus::Healthy);

        // Strikes 1-2: displayed status does not flap.
        assert_eq!(
            t.observe(HealthStatus::Unhealthy("x".into())),
            HealthStatus::Healthy
        );
        assert_eq!(
            t.observe(HealthStatus::Unhealthy("x".into())),
            HealthStatus::Healthy
        );
        // Strike 3: the failure is displayed.
        assert_eq!(
            t.observe(HealthStatus::Unhealthy("boom".into())),
            HealthStatus::Unhealthy("boom".into())
        );
        // One pass is not enough to recover.
        assert_eq!(
            t.observe(HealthStatus::Healthy),
            HealthStatus::Unhealthy("boom".into())
        );
        // Two consecutive passes recover.
        assert_eq!(t.observe(HealthStatus::Healthy), HealthStatus::Healthy);

        // A Degraded (non-fatal) result also counts as a strike toward display.
        let mut t2 = HealthTracker::new();
        t2.observe(HealthStatus::Healthy);
        t2.observe(HealthStatus::Degraded("warm".into()));
        t2.observe(HealthStatus::Degraded("warm".into()));
        assert_eq!(
            t2.observe(HealthStatus::Degraded("warm".into())),
            HealthStatus::Degraded("warm".into())
        );
    }

    #[tokio::test]
    async fn supervisor_probe_uses_registered_probe() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_probe(
            "svc",
            Arc::new(StubProbe(HealthStatus::Degraded("warming up".into()))),
        );
        assert!(matches!(
            sup.probe("svc").await.unwrap(),
            HealthStatus::Degraded(_)
        ));
        // Tracked probing applies the flapping guard (2 strikes: still Healthy).
        assert_eq!(
            sup.probe_tracked("svc").await.unwrap(),
            HealthStatus::Healthy
        );
        assert!(matches!(
            sup.probe("ghost").await,
            Err(SupervisorError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn wait_port_free_waits_for_release_and_times_out_while_held() {
        // Hold a port, then release it shortly after: the wait must
        // observe the release instead of failing on the first probe
        // (the restart-after-adopt race this helper exists for).
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_free(port));
        assert!(!wait_port_free(port, Duration::from_millis(200)).await);
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(listener);
        });
        assert!(wait_port_free(port, Duration::from_secs(5)).await);
        releaser.await.unwrap();
        // A free port returns immediately.
        assert!(wait_port_free(port, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn adopt_then_restart_captures_new_process_logs() {
        // The exact sequence from Pete's machine (2026-10-15): a
        // backend adopted from a previous session is restarted from
        // the Backends page; the NEW process's startup output must
        // land in logs_query under the backend's scope.
        let mut ext = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = ext.id();
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec(
            "svc",
            "echo 'svc startup: model loaded'; exec sleep 60",
        ));
        sup.adopt("svc", pid).unwrap();
        // The adopted process produced no captured output.
        assert!(sup.logs_query("svc", None, None, 10).records.is_empty());
        sup.restart("svc", false).await.unwrap();
        let _ = ext.wait();
        wait_for(
            || {
                sup.logs_query("svc", None, None, 10)
                    .records
                    .iter()
                    .any(|r| r.line.contains("model loaded"))
            },
            Duration::from_secs(5),
        )
        .await;
        let st = sup.status().into_iter().find(|s| s.id == "svc").unwrap();
        assert_eq!(st.state, ProcessState::Running);
        assert!(!st.adopted);
        sup.stop("svc", false).await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn logs_query_filters_search_limits_and_retains_after_stop() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec(
            "a",
            "echo 'alpha info: booted'; echo 'beta warn: slow disk'; echo 'gamma error: boom'",
        ));
        sup.spawn_backend("a").await.unwrap();
        wait_for(
            || sup.logs_query("a", None, None, 10).records.len() >= 3,
            Duration::from_secs(5),
        )
        .await;

        let all = sup.logs_query("a", None, None, 10);
        assert_eq!(all.records.len(), 3);
        assert_eq!(all.total_matching, 3);
        // Newest-first.
        assert!(all.records[0].line.contains("gamma"));
        assert!(all.records[2].line.contains("alpha"));
        // Records are stamped with their source id; nothing is archived.
        assert!(all.records.iter().all(|r| r.source == "a"));
        assert!(all.records.iter().all(|r| !r.archived));

        assert_eq!(
            sup.logs_query("a", Some("error"), None, 10).records.len(),
            1
        );
        assert_eq!(sup.logs_query("a", Some("warn"), None, 10).records.len(), 1);
        assert_eq!(sup.logs_query("a", Some("info"), None, 10).records.len(), 1);
        // Unknown level string = no filter.
        assert_eq!(
            sup.logs_query("a", Some("bogus"), None, 10).records.len(),
            3
        );

        let found = sup.logs_query("a", None, Some("ALPHA"), 10);
        assert_eq!(found.records.len(), 1);
        assert!(found.records[0].line.contains("alpha"));

        let limited = sup.logs_query("a", None, None, 2);
        assert_eq!(limited.records.len(), 2);
        assert_eq!(
            limited.total_matching, 3,
            "total counts matches before the limit binds"
        );
        assert!(limited.records[0].line.contains("gamma"));

        assert!(sup.logs_query("ghost", None, None, 10).records.is_empty());
        assert_eq!(sup.logs_query("ghost", None, None, 10).total_matching, 0);

        // Ring is retained after stop; "all" scope sees it.
        sup.stop("a", false).await.unwrap();
        let retained = sup.logs_query("a", None, None, 10);
        assert_eq!(retained.records.len(), 3);
        assert_eq!(sup.logs_query("all", None, None, 10).records.len(), 3);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn logs_query_all_merges_chronologically_and_reports_total() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec("a", "echo 'a-one'; echo 'a-two'; echo 'a-three'"));
        sup.register_spec(sh_spec("b", "echo 'b-one'; echo 'b-two'; echo 'b-three'"));
        sup.spawn_backend("a").await.unwrap();
        wait_for(
            || sup.logs_query("a", None, None, 10).records.len() >= 3,
            Duration::from_secs(5),
        )
        .await;
        // b starts strictly after a's lines were captured, so b's lines
        // are the globally newest.
        sup.spawn_backend("b").await.unwrap();
        wait_for(
            || sup.logs_query("all", None, None, 100).total_matching >= 6,
            Duration::from_secs(5),
        )
        .await;

        let merged = sup.logs_query("all", None, None, 100);
        assert_eq!(merged.total_matching, 6);
        assert_eq!(merged.records.len(), 6);
        // Chronological merge: all of b's lines precede a's, newest-first
        // within each process — the old ring-by-ring fill returned a's
        // block first regardless of timestamps.
        let lines: Vec<&str> = merged.records.iter().map(|r| r.line.as_str()).collect();
        assert_eq!(
            lines,
            vec!["b-three", "b-two", "b-one", "a-three", "a-two", "a-one"]
        );
        // Attribution survives the merge.
        assert!(merged.records[..3].iter().all(|r| r.source == "b"));
        assert!(merged.records[3..].iter().all(|r| r.source == "a"));

        // The limit applies to the merged stream: the page holds the
        // globally newest lines, not the first ring's.
        let page = sup.logs_query("all", None, None, 2);
        assert_eq!(page.records.len(), 2);
        assert_eq!(page.total_matching, 6);
        assert_eq!(page.records[0].line, "b-three");
        assert_eq!(page.records[1].line, "b-two");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn logs_survive_respawn_as_one_archived_generation() {
        let mut sup = Supervisor::new(test_policy());
        sup.register_spec(sh_spec("a", "echo 'run1 first'; echo 'run1 second'"));
        sup.spawn_backend("a").await.unwrap();
        wait_for(
            || sup.logs_query("a", None, None, 10).total_matching >= 2,
            Duration::from_secs(5),
        )
        .await;
        sup.stop("a", false).await.unwrap();

        // Respawn: run1's ring must survive as the archived previous run.
        sup.register_spec(sh_spec("a", "echo 'run2 first'; echo 'run2 second'"));
        sup.spawn_backend("a").await.unwrap();
        wait_for(
            || sup.logs_query("a", None, None, 10).total_matching >= 4,
            Duration::from_secs(5),
        )
        .await;

        let merged = sup.logs_query("a", None, None, 10);
        assert_eq!(merged.total_matching, 4);
        // Newest-first across generations: run2 (current) before run1.
        assert!(merged.records[0].line.contains("run2"));
        assert!(merged.records[3].line.contains("run1"));
        for rec in &merged.records {
            if rec.line.contains("run1") {
                assert!(rec.archived, "previous run must be flagged: {rec:?}");
            } else {
                assert!(!rec.archived, "current run must not be flagged: {rec:?}");
            }
            assert_eq!(rec.source, "a");
        }
        // The archived ring is part of the "all" merge too.
        assert_eq!(sup.logs_query("all", None, None, 10).total_matching, 4);
        // The scope picker sees the id with a previous run.
        let scopes = sup.log_scopes();
        let a = scopes.iter().find(|s| s.id == "a").expect("scope listed");
        assert!(a.previous);

        // A second respawn keeps exactly one archived generation: run1 is
        // gone, run2 becomes the archive.
        sup.stop("a", false).await.unwrap();
        sup.register_spec(sh_spec("a", "echo 'run3 only'"));
        sup.spawn_backend("a").await.unwrap();
        wait_for(
            || {
                sup.logs_query("a", None, None, 10)
                    .records
                    .iter()
                    .any(|r| r.line.contains("run3"))
            },
            Duration::from_secs(5),
        )
        .await;
        let third = sup.logs_query("a", None, None, 10);
        assert_eq!(third.total_matching, 3, "run3 (1) + archived run2 (2)");
        assert!(
            third.records.iter().all(|r| !r.line.contains("run1")),
            "oldest archive must be replaced: {:?}",
            third.records
        );
        assert!(third
            .records
            .iter()
            .filter(|r| r.line.contains("run2"))
            .all(|r| r.archived));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn reader_caps_overlong_lines() {
        // One 100 KiB unterminated-until-the-echo line, then a short line.
        let ring = test_ring();
        let mut proc = SupervisedProcess::spawn(
            "loud",
            &PathBuf::from("/bin/sh"),
            &[
                "-c".to_string(),
                "head -c 102400 /dev/zero | tr '\\0' 'x'; echo; echo after-line".to_string(),
            ],
            &[],
            Arc::clone(&ring),
        )
        .await
        .unwrap();
        wait_for(
            || ring_lines_chronological(&ring).len() >= 2,
            Duration::from_secs(5),
        )
        .await;
        proc.kill().await.unwrap();

        let lines = ring_lines_chronological(&ring);
        assert_eq!(lines.len(), 2);
        let long = &lines[0];
        assert!(long.starts_with("xxxx"), "capped content kept");
        assert!(
            long.len() <= MAX_LOG_LINE_BYTES + 64,
            "line must be capped near 64 KiB, got {} bytes",
            long.len()
        );
        assert!(
            long.contains("truncated"),
            "truncation must be marked: …{}",
            &long[long.len() - 80..]
        );
        // The reader survived the overlong line: the next line is intact.
        assert_eq!(lines[1], "after-line");
    }

    /// Read one HTTP request from `sock` (headers + body per Content-Length).
    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let body_len: usize = loop {
            let n = sock.read(&mut tmp).await.unwrap();
            assert!(n > 0, "client closed before sending a full request");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..hdr_end]);
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= hdr_end + 4 + len {
                    break len;
                }
            }
        };
        let _ = body_len;
        buf
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Spawn a hermetic fake HTTP server replying `status` + `body` once.
    /// Returns the base URL. Also asserts the request targeted
    /// `/v1/chat/completions` with a JSON body.
    async fn fake_chat_server(status: u16, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let req = read_http_request(&mut sock).await;
            let head = String::from_utf8_lossy(&req);
            assert!(
                head.starts_with("POST /v1/chat/completions "),
                "unexpected request line: {}",
                head.lines().next().unwrap_or("")
            );
            assert!(head.contains("content-type: application/json"));
            let resp = format!(
                "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            use tokio::io::AsyncWriteExt;
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Spawn a hermetic fake backend serving the tool-call probe's
    /// three requests in order: `GET /health`, `GET /props`, then
    /// `POST /v1/chat/completions`. Returns the base URL. The chat
    /// request is asserted against the probe's requirements (probe
    /// tool name, a defined property, `tool_choice: "required"`,
    /// thinking disabled). Fewer requests (the health-gate case)
    /// simply leave the spawned task waiting on accept.
    async fn fake_tool_probe_server(
        health_status: u16,
        props_body: &str,
        chat_body: &str,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let props_body = props_body.to_string();
        let chat_body = chat_body.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for _ in 0..3 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let req = read_http_request(&mut sock).await;
                let head = String::from_utf8_lossy(&req).into_owned();
                let first = head.lines().next().unwrap_or("").to_string();
                let (status, body) = if first.starts_with("GET /health ") {
                    (health_status, "ok".to_string())
                } else if first.starts_with("GET /props ") {
                    (200, props_body.clone())
                } else if first.starts_with("POST /v1/chat/completions ") {
                    assert!(
                        head.contains(TOOL_CALL_PROBE_TOOL_NAME),
                        "probe request must offer the probe tool: {head}"
                    );
                    assert!(
                        head.contains("\"city\""),
                        "probe tool must define a property: {head}"
                    );
                    assert!(
                        head.contains("\"tool_choice\":\"required\""),
                        "probe request must require a tool call: {head}"
                    );
                    assert!(
                        head.contains("\"enable_thinking\":false"),
                        "probe request must disable thinking: {head}"
                    );
                    (200, chat_body.clone())
                } else {
                    panic!("unexpected request line: {first}");
                };
                let resp = format!(
                    "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    const PROPS_WITH_TOOL_USE: &str =
        r#"{"chat_template_caps":{"supports_tools":true,"chat_template_tool_use":true}}"#;

    #[tokio::test]
    async fn test_tool_call_verifies_structured_tool_call() {
        let base = fake_tool_probe_server(
            200,
            PROPS_WITH_TOOL_USE,
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"probe_get_weather","arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .await;
        let r = test_tool_call(&base).await.unwrap();
        assert_eq!(r.outcome, ToolCallProbeOutcome::Verified);
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert!(r.chat_template_tool_use);
        assert_eq!(
            r.chat_template_caps.as_ref().and_then(|c| c["supports_tools"].as_bool()),
            Some(true)
        );
        assert!(r.detail.contains("probe_get_weather"), "{}", r.detail);
    }

    #[tokio::test]
    async fn test_tool_call_unparsed_when_markup_leaks_into_content() {
        let base = fake_tool_probe_server(
            200,
            r#"{"chat_template_caps":{"supports_tools":false}}"#,
            r#"{"choices":[{"message":{"role":"assistant","content":"<tool_call>{\"name\":\"probe_get_weather\",\"arguments\":{\"city\":\"Paris\"}}</tool_call>"},"finish_reason":"stop"}]}"#,
        )
        .await;
        let r = test_tool_call(&base).await.unwrap();
        assert_eq!(r.outcome, ToolCallProbeOutcome::Unparsed);
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        // No chat_template_tool_use entry was reported.
        assert!(!r.chat_template_tool_use);
        assert!(r.chat_template_caps.is_some());
    }

    #[tokio::test]
    async fn test_tool_call_failed_on_plain_content() {
        let base = fake_tool_probe_server(
            200,
            PROPS_WITH_TOOL_USE,
            r#"{"choices":[{"message":{"role":"assistant","content":"The weather in Paris is sunny."},"finish_reason":"stop"}]}"#,
        )
        .await;
        let r = test_tool_call(&base).await.unwrap();
        assert_eq!(r.outcome, ToolCallProbeOutcome::Failed);
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert!(r.detail.contains("no tool call"), "{}", r.detail);
    }

    #[tokio::test]
    async fn test_tool_call_failed_at_health_gate() {
        // /health is not 2xx: the probe ends as Failed before any
        // /props or chat request is made.
        let base = fake_tool_probe_server(503, PROPS_WITH_TOOL_USE, "{}").await;
        let r = test_tool_call(&base).await.unwrap();
        assert_eq!(r.outcome, ToolCallProbeOutcome::Failed);
        assert!(r.detail.contains("health"), "{}", r.detail);
        assert_eq!(r.finish_reason, None);
    }

    #[tokio::test]
    async fn test_request_parses_chat_completion() {
        let base = fake_chat_server(
            200,
            r#"{"id":"t","choices":[{"message":{"content":"hello back"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        )
        .await;
        let r = test_request(&base, "hi").await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content.as_deref(), Some("hello back"));
        assert_eq!(r.prompt_tokens, Some(5));
        assert_eq!(r.completion_tokens, Some(2));
        assert!(r.latency_ms < 5000);
    }

    #[test]
    fn chat_completion_interprets_a_plain_answer() {
        let body = r#"{"choices":[{"message":{"content":"{\"summary\":\"ok\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1200,"completion_tokens":300}}"#;
        let r = interpret_chat_completion(200, body, 42, 4096).unwrap();
        assert_eq!(r.content, "{\"summary\":\"ok\"}");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!(r.prompt_tokens, Some(1200));
        assert_eq!(r.completion_tokens, Some(300));
        assert_eq!(r.http_status, 200);
        assert_eq!(r.response_bytes, body.len());
        assert_eq!(r.reasoning_bytes, 0);
    }

    #[test]
    fn chat_completion_interprets_content_parts() {
        // The array-of-parts message shape some servers emit:
        // the text parts join into the answer.
        let body = r#"{"choices":[{"message":{"content":[{"type":"text","text":"{\"sum"},{"type":"text","text":"mary\":\"ok\"}"}]},"finish_reason":"stop"}]}"#;
        let r = interpret_chat_completion(200, body, 1, 4096).unwrap();
        assert_eq!(r.content, "{\"summary\":\"ok\"}");
    }

    #[test]
    fn chat_completion_refuses_a_reasoning_burned_budget() {
        // Pete's Oct 19, 2026 field failure: the model spent the
        // whole token budget on reasoning and produced no answer
        // text at all. That must be an error naming the cause —
        // never a successful empty completion.
        let body = r#"{"choices":[{"message":{"content":"","reasoning_content":"thinking at length about the plan"},"finish_reason":"length"}],"usage":{"prompt_tokens":4500,"completion_tokens":4096}}"#;
        let err = interpret_chat_completion(200, body, 200_000, 4096).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("spent the full 4096-token budget on reasoning"), "{msg}");
        assert!(msg.contains("finish_reason length"), "{msg}");
        assert!(msg.contains("completion_tokens 4096"), "{msg}");
    }

    #[test]
    fn chat_completion_refuses_reasoning_without_an_answer() {
        let body = r#"{"choices":[{"message":{"content":null,"reasoning_content":"some reasoning"},"finish_reason":"stop"}],"usage":{"completion_tokens":50}}"#;
        let err = interpret_chat_completion(200, body, 900, 4096).unwrap_err();
        assert!(err.to_string().contains("only reasoning"), "{err}");
    }

    #[test]
    fn chat_completion_refuses_an_empty_completion() {
        let body = r#"{"choices":[{"message":{"content":""},"finish_reason":"stop"}],"usage":{"completion_tokens":0}}"#;
        let err = interpret_chat_completion(200, body, 5, 4096).unwrap_err();
        assert!(err.to_string().contains("empty completion"), "{err}");
    }

    #[test]
    fn chat_completion_refuses_a_missing_first_choice() {
        let err = interpret_chat_completion(200, r#"{"choices":[]}"#, 5, 4096).unwrap_err();
        assert!(err.to_string().contains("no first-choice message"), "{err}");
    }

    #[tokio::test]
    async fn chat_completion_surfaces_the_reasoning_burn_as_an_error() {
        let base = fake_chat_server(
            200,
            r#"{"choices":[{"message":{"content":"","reasoning_content":"reasoning"},"finish_reason":"length"}],"usage":{"prompt_tokens":4500,"completion_tokens":4096}}"#,
        )
        .await;
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "plan please".to_string(),
        }];
        let err = chat_completion(&base, "m", &messages, 0.2, 4096, Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("spent the full 4096-token budget"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_request_tolerates_trailing_slash_and_missing_usage() {
        let base = fake_chat_server(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
        let r = test_request(&format!("{base}/"), "hi").await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content.as_deref(), Some("ok"));
        assert_eq!(r.prompt_tokens, None);
    }

    #[tokio::test]
    async fn test_request_reports_non_2xx_without_content() {
        let base = fake_chat_server(500, r#"{"error":"overloaded"}"#).await;
        let r = test_request(&base, "hi").await.unwrap();
        assert_eq!(r.status, 500);
        assert!(r.content.is_none());
    }

    /// Spawn a hermetic fake HTTP server replying once with the
    /// given content type and body (SSE-shaped when the type says
    /// so). Returns the base URL.
    async fn fake_typed_server(content_type: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let req = read_http_request(&mut sock).await;
            let head = String::from_utf8_lossy(&req);
            assert!(head.starts_with("POST /v1/chat/completions "));
            // The benchmark request must ask for streaming.
            assert!(head.contains("\"stream\":true"), "request: {head}");
            let resp = format!(
                "HTTP/1.1 200 test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            use tokio::io::AsyncWriteExt;
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn benchmark_completion_reads_sse_timings_and_ttft() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"timings\":{\"prompt_per_second\":900.5,\"predicted_per_second\":25.25,\"prompt_ms\":44.0,\"prompt_n\":40,\"predicted_ms\":1000.0,\"predicted_n\":25}}\n\n",
            "data: [DONE]\n\n",
        );
        let base = fake_typed_server("text/event-stream", sse).await;
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "benchmark".to_string(),
        }];
        let r = benchmark_completion(&base, "m", &messages, 0.0, 256, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(r.content, "Hello world");
        assert_eq!(r.prompt_per_second, Some(900.5));
        assert_eq!(r.predicted_per_second, Some(25.25));
        assert!(r.ttft_ms.is_some(), "first content chunk sets TTFT");
        assert!(r.ttft_ms.unwrap() >= 0.0);
    }

    #[tokio::test]
    async fn benchmark_completion_falls_back_to_plain_json() {
        let base = fake_chat_server(
            200,
            r#"{"choices":[{"message":{"content":"plain answer"}}],"timings":{"prompt_ms":100.0,"prompt_n":50,"predicted_ms":2000.0,"predicted_n":40}}"#,
        )
        .await;
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "benchmark".to_string(),
        }];
        let r = benchmark_completion(&base, "m", &messages, 0.0, 128, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(r.content, "plain answer");
        // No event stream: TTFT stays absent, never estimated.
        assert_eq!(r.ttft_ms, None);
        // Rates derived from the ms/token counts when the direct
        // per-second fields are absent: 50 / 0.1 s, 40 / 2 s.
        assert_eq!(r.prompt_per_second, Some(500.0));
        assert_eq!(r.predicted_per_second, Some(20.0));
    }

    #[tokio::test]
    async fn test_request_transport_failure_is_http_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = test_request(&format!("http://127.0.0.1:{port}"), "hi")
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::Http(_)));
    }

    #[test]
    fn log_level_filter_parse_and_match() {
        assert_eq!(LogLevelFilter::parse("error"), Some(LogLevelFilter::Error));
        assert_eq!(LogLevelFilter::parse("WARN"), Some(LogLevelFilter::Warn));
        assert_eq!(LogLevelFilter::parse("bogus"), None);
        assert!(LogLevelFilter::Error.matches_line("PANIC: something failed"));
        assert!(LogLevelFilter::Warn.matches_line("warning: slow"));
        assert!(!LogLevelFilter::Warn.matches_line("all good"));
    }
}
