//! Production [`SystemOps`](crate::SystemOps) implementation.
//!
//! Every method shells out to the real system: `nvidia-smi` for the GPU
//! inventory, `tokio::process` for commands, `reqwest` for downloads and
//! HTTP probes, `zip` for archive extraction. All failures are mapped into
//! [`WizardError`](crate::WizardError); nothing panics on missing tools.
//!
//! Network calls carry timeouts so a hung peer cannot hang the wizard:
//! health probes fail fast, downloads fail fast on connect (a stalled
//! mid-download is surfaced by the progress callback going quiet, which the
//! UI renders — reqwest has no stall timeout to set here).

use crate::{CommandOutput, GpuDescriptor, HttpResponse, PlannedCommand, SystemOps, WizardError};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fail fast when the server cannot be reached at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum gap between body chunks on a download before the
/// connection is treated as stalled (see the download loop).
const CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Health probes must answer quickly; the retry loop handles slowness.
const HTTP_GET_TIMEOUT: Duration = Duration::from_secs(15);
/// Default budget for one planned command. Package installs
/// (`uv pip install`) and Prisma's first-run toolchain download are
/// the slow legitimate cases; anything past this is a stall. Before
/// this bound existed, a single stalled child held the wizard lock
/// for the rest of the session and every later step run reported
/// "busy" (2026-10-16). Callers with a genuinely longer job use
/// [`SystemOps::run_command_with_timeout`].
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Budget for an elevated (UAC) command: the firewall script is a
/// few `netsh` calls. The process is not killed on expiry — the
/// caller treats this failure as warning-only and the script is
/// left to finish on its own.
#[cfg(windows)]
const ELEVATED_WAIT_TIMEOUT_MS: u32 = 120_000;
/// Above `SMOKE_MAX_LATENCY_MS` (120 s) so the smoke step — not the client —
/// decides what "too slow" means.
const HTTP_POST_TIMEOUT: Duration = Duration::from_secs(180);

/// Production system effects.
#[derive(Debug, Default)]
pub struct RealSystemOps {
    /// Set by the step runner when a step's budget expires; checked
    /// by the blocking extraction loops, which run on threads a
    /// dropped future cannot cancel. Cleared when a step starts.
    step_cancel: Arc<AtomicBool>,
}

impl RealSystemOps {
    fn io_err(path: &Path, e: impl std::fmt::Display) -> WizardError {
        WizardError::IoError {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }
    }

    /// Parse `nvidia-smi --query-gpu=... --format=csv,noheader` rows.
    ///
    /// Expected row: `0, NVIDIA RTX A4000, GPU-uuid, 16384 MiB, 8.6`
    fn parse_gpu_rows(text: &str, driver: &str) -> Vec<GpuDescriptor> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                // GPU names do not contain commas; split into exactly 5 fields.
                let mut parts = line.splitn(5, ',').map(str::trim);
                let index: u32 = parts.next()?.parse().ok()?;
                let name = parts.next()?.to_string();
                let uuid = parts.next()?.to_string();
                let vram: u64 = parts.next()?.split_whitespace().next()?.parse().ok()?;
                let cap = parts.next()?;
                let mut cc = cap.split('.');
                let major: u32 = cc.next()?.parse().ok()?;
                let minor: u32 = cc.next().unwrap_or("0").parse().ok()?;
                Some(GpuDescriptor {
                    index,
                    name,
                    uuid,
                    total_vram_mib: vram,
                    compute_capability: (major, minor),
                    driver_version: driver.to_string(),
                })
            })
            .collect()
    }

    /// Parse the `Driver Version: 581.57` line from plain `nvidia-smi` output.
    fn parse_driver_version(text: &str) -> String {
        for line in text.lines() {
            if let Some(idx) = line.find("Driver Version:") {
                let rest = line[idx + "Driver Version:".len()..].trim();
                if let Some(ver) = rest.split_whitespace().next() {
                    return ver.to_string();
                }
            }
        }
        "0.0".to_string()
    }
}

/// A pid is only safe to signal when it names one specific process:
/// 0 means "my own process group" and a value above `i32::MAX` wraps
/// to a negative `pid_t` — -1 is "every process I own". A corrupt or
/// hand-edited pid file must never turn a liveness check or a stop
/// into a broadcast signal, so out-of-range pids are refused here,
/// at the only place pids reach the OS.
fn pid_is_signalable(pid: u32) -> bool {
    pid >= 1 && pid <= i32::MAX as u32
}

/// Whether a process with `pid` is currently alive.
///
/// Windows: `tasklist` filtered to the pid (same shell-out approach as
/// the cmdkey presence checks elsewhere in this crate — no extra FFI).
/// Unix: `kill(pid, 0)` via `/bin/kill -0`.
pub fn process_alive(pid: u32) -> bool {
    if !pid_is_signalable(pid) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/NH"]);
        // CREATE_NO_WINDOW: this probe runs from a GUI process.
        cmd.creation_flags(0x08000000);
        cmd.output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}

/// Terminate the process with `pid`.
///
/// Windows: `taskkill /PID <pid> /T /F` — tree semantics, because a
/// launcher's children must die with it. Unix: SIGTERM to the bare
/// pid via `/bin/kill`. The recorded gateway pid IS the server (the
/// start-gateway step spawns the venv's python directly), so no tree
/// walk is needed; the negative-pid group form is deliberately not
/// used — it targets whatever group numerically matches the pid,
/// which is only the target's own group for session leaders and is
/// an unacceptable blast radius for a pid read from a file.
pub fn kill_process(pid: u32) -> std::io::Result<()> {
    if !pid_is_signalable(pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to signal out-of-range pid {pid}"),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        // CREATE_NO_WINDOW: this runs from a GUI process.
        cmd.creation_flags(0x08000000);
        let out = cmd.output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "taskkill /PID {pid} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
    #[cfg(unix)]
    {
        let status = std::process::Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "kill {pid} failed with {status}"
            )))
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "process termination is not supported on this platform",
        ))
    }
}

/// Path of the file recording the running gateway's pid:
/// `<data_dir>/gateway.pid`. Written by the start-gateway step after
/// each successful spawn; read by [`stop_gateway`] and by the step
/// itself to stop the previous gateway before spawning a replacement.
pub fn gateway_pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway.pid")
}

/// Stop the gateway process recorded in `<data_dir>/gateway.pid`.
///
/// Returns `Ok(true)` when a live recorded process was terminated,
/// `Ok(false)` when there was nothing to stop (no pid file, an
/// unparsable pid, or the process already gone). The pid file is
/// removed in both cases; it is kept only when the kill itself fails,
/// so a retry still knows which pid to target.
pub fn stop_gateway(data_dir: &Path) -> Result<bool, String> {
    let pid_path = gateway_pid_path(data_dir);
    let text = match std::fs::read_to_string(&pid_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("could not read {}: {e}", pid_path.display())),
    };
    let pid: Option<u32> = text.trim().parse().ok();
    let stopped = match pid {
        Some(pid) if process_alive(pid) => {
            kill_process(pid).map_err(|e| format!("could not stop gateway (pid {pid}): {e}"))?;
            true
        }
        _ => false,
    };
    let _ = std::fs::remove_file(&pid_path);
    Ok(stopped)
}

/// Kill the whole process tree rooted at `pid`, best-effort.
///
/// Windows: `kill_process` is already `taskkill /T /F` (tree
/// semantics). Unix: commands spawned by [`RealSystemOps::run_child`]
/// lead their own process group, so SIGKILL to the group takes the
/// children with the parent; if the group signal fails (the child
/// was not a group leader), fall back to killing the bare pid.
fn kill_process_tree(pid: u32) {
    if !pid_is_signalable(pid) {
        return;
    }
    #[cfg(windows)]
    {
        let _ = kill_process(pid);
    }
    #[cfg(unix)]
    {
        // SAFETY: killpg with a validated positive pid signals only
        // that process group.
        let rc = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        if rc != 0 {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// RAII guard that kills a spawned command's process tree when
/// dropped while still armed. `run_child` arms it at spawn and
/// disarms it only after the child has been reaped, so every exit
/// path that loses track of the child — a budget timeout (the wait
/// future is dropped), a dropped step future (the step itself timed
/// out), a wait error — kills the tree instead of leaving a
/// half-finished installer or toolchain running detached.
struct TreeKillGuard {
    pid: Option<u32>,
}

impl TreeKillGuard {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
    }
}

/// Shared GET implementation behind the trait's plain and
/// header-carrying variants.
/// Quote one argument for a ShellExecuteExW parameter string: wrap in
/// double quotes when it contains whitespace or a quote, escaping
/// embedded quotes. Good enough for the paths and flags the wizard
/// launches elevated (a .cmd path and subcommand words).
#[cfg(windows)]
fn shell_quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Launch `cmd` elevated via ShellExecuteExW with the "runas" verb
/// (one UAC prompt), wait for the process to exit, and return its exit
/// code. The user declining the prompt makes ShellExecuteExW fail,
/// which surfaces as an IoError naming the program.
#[cfg(windows)]
fn run_elevated_windows(cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let program = cmd.argv.first().cloned().unwrap_or_default();
    let params = cmd
        .argv
        .iter()
        .skip(1)
        .map(|a| shell_quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = program.encode_utf16().chain(std::iter::once(0)).collect();
    let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = SW_HIDE;
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: format!("elevation failed or was declined (ShellExecuteEx error {code})"),
        });
    }
    // Bounded wait (this was INFINITE): a stalled elevated script
    // must not hang the calling step for the rest of the session.
    // WAIT_TIMEOUT (0x102) means the budget expired; the handle is
    // closed and an error returned, and the elevated process is
    // left to finish — the only caller (the gateway firewall rule)
    // treats this failure as warning-only.
    const WAIT_TIMEOUT_CODE: u32 = 0x0000_0102;
    const WAIT_OBJECT_0: u32 = 0;
    let wait_result = unsafe { WaitForSingleObject(info.hProcess, ELEVATED_WAIT_TIMEOUT_MS) };
    if wait_result == WAIT_TIMEOUT_CODE {
        unsafe { CloseHandle(info.hProcess) };
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: format!(
                "elevated command did not finish within {} seconds",
                ELEVATED_WAIT_TIMEOUT_MS / 1000
            ),
        });
    }
    // Anything other than "signaled" (WAIT_FAILED, an abandoned
    // mutex, ...) means the wait itself broke down; falling through
    // used to read an uninitialized exit code and report success.
    if wait_result != WAIT_OBJECT_0 {
        unsafe { CloseHandle(info.hProcess) };
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: format!(
                "waiting for the elevated command failed (wait result {wait_result:#x})"
            ),
        });
    }
    let mut exit_code: u32 = 0;
    let got = unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) };
    unsafe { CloseHandle(info.hProcess) };
    if got == 0 {
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: "could not read the elevated command's exit code".to_string(),
        });
    }
    Ok(CommandOutput {
        exit_code: exit_code as i32,
        stdout: String::new(),
        stderr: String::new(),
    })
}

async fn http_get_impl(
    url: &str,
    headers: &[(String, String)],
) -> Result<HttpResponse, WizardError> {
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(HTTP_GET_TIMEOUT)
        .build()
        .map_err(|e| WizardError::HttpError {
            url: url.to_string(),
            reason: format!("client build failed: {e}"),
        })?;
    let mut req = client.get(url);
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    Ok(HttpResponse {
        status,
        body: body.to_vec(),
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

/// Shared POST implementation behind the trait's plain and
/// header-carrying variants.
async fn http_post_impl(
    url: &str,
    body: &str,
    headers: &[(String, String)],
) -> Result<HttpResponse, WizardError> {
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(HTTP_POST_TIMEOUT)
        .build()
        .map_err(|e| WizardError::HttpError {
            url: url.to_string(),
            reason: format!("client build failed: {e}"),
        })?;
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    Ok(HttpResponse {
        status,
        body: bytes.to_vec(),
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

/// The rotation sibling of a log file: `gateway.stdout.log` rotates
/// to `gateway.stdout.log.prev` (one previous run kept, matching the
/// log retention the Logs view documents).
pub fn prev_log_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".prev");
    PathBuf::from(s)
}

/// Open `path` for appending, first rotating any existing file to its
/// `.prev` sibling. Creates parent directories as needed.
fn open_rotated_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let prev = prev_log_path(path);
        let _ = std::fs::remove_file(&prev);
        std::fs::rename(path, &prev)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

impl RealSystemOps {
    /// Spawn `cmd` and wait for it under `budget`, capturing output.
    ///
    /// The budget is the ONLY timeout applied: `run_command` passes
    /// the default [`COMMAND_TIMEOUT`], `run_command_with_timeout`
    /// passes the caller's figure through untouched. Before this
    /// split, the with-timeout variant inherited the trait default,
    /// which wrapped `run_command` — whose own 600 s cap fired first
    /// and made the callers' 900 s install budgets unreachable
    /// (code-review 2.1).
    ///
    /// On expiry the whole process TREE is killed, not just the
    /// direct child: installers spawn children of their own (uv →
    /// pip → compiler), and killing only the parent left a
    /// half-finished toolchain writing into the tree the step's
    /// retry was about to reuse. The [`TreeKillGuard`] also covers
    /// the future itself being dropped (the step-level timeout):
    /// the child leads its own process group on Unix and is a
    /// taskkill tree root on Windows either way.
    async fn run_child(
        &self,
        cmd: &PlannedCommand,
        budget: Duration,
    ) -> Result<CommandOutput, WizardError> {
        let (prog, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| WizardError::CommandFailed {
                argv: cmd.argv.clone(),
                code: -1,
                stderr: "empty argv".to_string(),
            })?;
        let mut c = tokio::process::Command::new(prog);
        c.args(args);
        if let Some(cwd) = &cmd.cwd {
            c.current_dir(cwd);
        }
        for (k, v) in &cmd.env {
            c.env(k, v);
        }
        // Planned commands (uv, nvidia-smi probes, installers) all run
        // headless from the GUI app; on Windows each spawn would open
        // a visible console window without CREATE_NO_WINDOW.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW
            c.creation_flags(0x08000000);
        }
        // Own process group on Unix, so the tree kill can signal the
        // group the child leads (kill_process_tree).
        #[cfg(unix)]
        c.process_group(0);
        // The guard, not kill_on_drop, owns termination: it kills
        // the TREE. kill_on_drop stays off so a timeout cannot kill
        // the parent a moment before the tree walk needs it as the
        // root. Every path out of this function either reaps the
        // child (guard disarmed) or drops the guard (tree killed).
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        let child = c.spawn().map_err(|e| WizardError::CommandFailed {
            argv: cmd.argv.clone(),
            code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
        let mut guard = TreeKillGuard {
            pid: child.id().filter(|p| pid_is_signalable(*p)),
        };
        let out = match tokio::time::timeout(budget, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Err(WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("wait failed: {e}"),
                })
            }
            Err(_) => {
                // The wait future is dropped with the child handle;
                // the armed guard kills the tree.
                return Err(WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("timed out after {}s and was stopped", budget.as_secs()),
                });
            }
        };
        guard.disarm();
        Ok(CommandOutput {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// The step-cancellation flag, cloned into blocking work (the
    /// extractors run on `spawn_blocking` threads where `self` is
    /// unavailable): they check it per entry and fail with
    /// `WizardError::Cancelled`.
    fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.step_cancel.clone()
    }
}

#[async_trait]
impl SystemOps for RealSystemOps {
    fn note_step_started(&self) {
        self.step_cancel.store(false, Ordering::SeqCst);
    }

    fn request_cancellation(&self) {
        self.step_cancel.store(true, Ordering::SeqCst);
    }

    async fn run_command(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
        self.run_child(cmd, COMMAND_TIMEOUT).await
    }

    async fn run_command_with_timeout(
        &self,
        cmd: &PlannedCommand,
        timeout: Duration,
    ) -> Result<CommandOutput, WizardError> {
        // The caller's budget is honored exactly — no inner default
        // cap underneath it (code-review 2.1).
        self.run_child(cmd, timeout).await
    }

    async fn service_start_gateway(
        &self,
        def: &manager_services::ServiceDefinition,
    ) -> Result<u32, WizardError> {
        #[cfg(windows)]
        {
            use manager_services::ServiceControl;
            let control = manager_services::scm::platform_control();
            let name = def.name.clone();
            let pid_file = def.pid_file.clone();
            // SCM calls are synchronous; keep them off the async runtime.
            tokio::task::spawn_blocking(move || {
                if !control.is_registered(&name).map_err(|e| e.to_string())? {
                    return Err(format!(
                        "the gateway service '{name}' is not registered — register it in Settings → Supervision before starting it in service mode"
                    ));
                }
                control.start(&name).map_err(|e| e.to_string())?;
                if let Some(pid_file) = &pid_file {
                    // The service host writes the child's pid once it
                    // is spawned; wait for a live pid to appear.
                    for _ in 0..150 {
                        if let Ok(text) = std::fs::read_to_string(pid_file) {
                            if let Ok(pid) = text.trim().parse::<u32>() {
                                if process_alive(pid) {
                                    return Ok(pid);
                                }
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(format!(
                        "gateway service '{name}' started but its process did not appear (no live pid in {})",
                        pid_file.display()
                    ))
                } else {
                    control
                        .query(&name)
                        .map_err(|e| e.to_string())?
                        .pid
                        .ok_or_else(|| {
                            format!("gateway service '{name}' has no process id after start")
                        })
                }
            })
            .await
            .map_err(|e| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!("service start task failed: {e}"),
            })?
            .map_err(|reason| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason,
            })
        }
        #[cfg(not(windows))]
        {
            Err(WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!(
                    "service supervision requires Windows (service '{}')",
                    def.name
                ),
            })
        }
    }

    async fn service_stop(&self, name: &str) -> Result<(), WizardError> {
        #[cfg(windows)]
        {
            use manager_services::ServiceControl;
            let control = manager_services::scm::platform_control();
            let name = name.to_string();
            tokio::task::spawn_blocking(move || {
                if !control.is_registered(&name).map_err(|e| e.to_string())? {
                    return Ok(());
                }
                control.stop(&name).map_err(|e| e.to_string())?;
                for _ in 0..150 {
                    match control.query(&name).map_err(|e| e.to_string())?.state {
                        manager_services::ServiceState::Stopped
                        | manager_services::ServiceState::NotInstalled => return Ok(()),
                        _ => std::thread::sleep(std::time::Duration::from_millis(100)),
                    }
                }
                Err(format!(
                    "service '{name}' did not reach the stopped state within 15 s"
                ))
            })
            .await
            .map_err(|e| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!("service stop task failed: {e}"),
            })?
            .map_err(|reason| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason,
            })
        }
        #[cfg(not(windows))]
        {
            Err(WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!(
                    "service supervision requires Windows (service '{name}')"
                ),
            })
        }
    }

    async fn spawn_detached(&self, cmd: &PlannedCommand) -> Result<u32, WizardError> {
        let (prog, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| WizardError::CommandFailed {
                argv: cmd.argv.clone(),
                code: -1,
                stderr: "empty argv".to_string(),
            })?;
        let mut c = tokio::process::Command::new(prog);
        c.args(args);
        // The planned environment must reach the child: the gateway's
        // OpenRouter key travels exclusively this way (the generated
        // YAML references it as os.environ/OPENROUTER_API_KEY). Dropping
        // it here silently disarms the fallback in production.
        for (k, v) in &cmd.env {
            c.env(k, v);
        }
        // Detach from this process group so the child outlives the wizard.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW | DETACHED_PROCESS
            c.creation_flags(0x08000000 | 0x00000008);
        }
        #[cfg(not(windows))]
        {
            // New session: the child is not in our process group.
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        // Capture the child's output when the caller named log files:
        // a detached process with null stdio fails invisibly — when
        // the gateway dies at startup, its own error output is the
        // only place the cause exists. A failure to open the log is
        // reported, not silently degraded to discarding output.
        let stdout_stdio = match &cmd.stdout_log {
            Some(path) => std::process::Stdio::from(open_rotated_log(path).map_err(|e| {
                WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("could not open stdout log {}: {e}", path.display()),
                }
            })?),
            None => std::process::Stdio::null(),
        };
        let stderr_stdio = match &cmd.stderr_log {
            Some(path) => std::process::Stdio::from(open_rotated_log(path).map_err(|e| {
                WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("could not open stderr log {}: {e}", path.display()),
                }
            })?),
            None => std::process::Stdio::null(),
        };
        c.stdin(std::process::Stdio::null())
            .stdout(stdout_stdio)
            .stderr(stderr_stdio)
            .kill_on_drop(false);
        let child = c.spawn().map_err(|e| WizardError::CommandFailed {
            argv: cmd.argv.clone(),
            code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
        Ok(child.id().unwrap_or(0))
    }

    fn process_alive(&self, pid: u32) -> bool {
        process_alive(pid)
    }

    fn kill_process(&self, pid: u32) -> std::io::Result<()> {
        kill_process(pid)
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<u64, WizardError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| WizardError::DownloadFailed {
                url: url.to_string(),
                reason: format!("client build failed: {e}"),
            })?;
        let mut resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| WizardError::DownloadFailed {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        if !resp.status().is_success() {
            return Err(WizardError::DownloadFailed {
                url: url.to_string(),
                reason: format!("HTTP {}", resp.status()),
            });
        }
        let total = resp.content_length().unwrap_or(0);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Self::io_err(dest, e))?;
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| Self::io_err(dest, e))?;
        let mut downloaded: u64 = 0;
        loop {
            // No-data timeout: the client has a connect timeout but a
            // connection that stalls mid-body would otherwise block
            // the step (and the wizard lock) forever with no output —
            // the install-postgres hang Pete hit on 2026-10-15. A
            // slow but progressing download never trips this.
            let chunk = match tokio::time::timeout(CHUNK_IDLE_TIMEOUT, resp.chunk()).await {
                Ok(r) => r.map_err(|e| WizardError::DownloadFailed {
                    url: url.to_string(),
                    reason: e.to_string(),
                })?,
                Err(_) => {
                    return Err(WizardError::DownloadFailed {
                        url: url.to_string(),
                        reason: format!(
                            "no data received for {} s; the connection appears stalled",
                            CHUNK_IDLE_TIMEOUT.as_secs()
                        ),
                    })
                }
            };
            let Some(bytes) = chunk else { break };
            tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
                .await
                .map_err(|e| Self::io_err(dest, e))?;
            downloaded += bytes.len() as u64;
            on_progress(downloaded, total);
        }
        Ok(downloaded)
    }

    async fn extract_zip(&self, archive: &Path, dest: &Path) -> Result<(), WizardError> {
        let archive_c = archive.to_path_buf();
        let dest_c = dest.to_path_buf();
        let cancel = self.cancel_flag();
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| Self::io_err(&archive_c, e))?;
            // Manual per-entry loop (not `zip.extract`) so a step
            // cancellation lands between entries: extraction runs on
            // a blocking thread the step's dropped future cannot
            // cancel, and a timed-out extraction otherwise kept
            // writing into the tree the retry was extracting into.
            // `enclosed_name` gives the same escape protection
            // `extract` applies internally.
            for i in 0..zip.len() {
                if cancel.load(Ordering::SeqCst) {
                    return Err(WizardError::Cancelled);
                }
                let mut entry = zip.by_index(i).map_err(|e| Self::io_err(&archive_c, e))?;
                let Some(rel_path) = entry.enclosed_name() else {
                    return Err(WizardError::IoError {
                        path: archive_c.clone(),
                        reason: format!("zip entry escapes the destination: {}", entry.name()),
                    });
                };
                let target = dest_c.join(&rel_path);
                if entry.is_dir() {
                    std::fs::create_dir_all(&target).map_err(|e| Self::io_err(&target, e))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Self::io_err(parent, e))?;
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| Self::io_err(&archive_c, e))?;
                std::fs::write(&target, &bytes).map_err(|e| Self::io_err(&target, e))?;
                // `zip.extract` applied the entry's Unix mode; the
                // manual loop must too — the extracted uv binary is
                // not runnable without its executable bit.
                #[cfg(unix)]
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ =
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn extract_tar_gz(
        &self,
        archive: &Path,
        dest: &Path,
        prefix: &str,
        strip_prefix: &str,
    ) -> Result<(), WizardError> {
        let archive_c = archive.to_path_buf();
        let dest_c = dest.to_path_buf();
        let prefix_c = prefix.to_string();
        let strip_c = strip_prefix.to_string();
        let cancel = self.cancel_flag();
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let gz = flate2::read::GzDecoder::new(file);
            let mut tar = tar::Archive::new(gz);
            let entries = tar.entries().map_err(|e| Self::io_err(&archive_c, e))?;
            for entry in entries {
                if cancel.load(Ordering::SeqCst) {
                    return Err(WizardError::Cancelled);
                }
                let mut entry = entry.map_err(|e| Self::io_err(&archive_c, e))?;
                let path = entry.path().map_err(|e| Self::io_err(&archive_c, e))?;
                let path_str = path.to_string_lossy().replace('\\', "/");
                if !path_str.starts_with(&prefix_c) {
                    continue;
                }
                let Some(rel) = path_str.strip_prefix(&strip_c) else {
                    continue;
                };
                if rel.is_empty() {
                    continue;
                }
                let rel_path = Path::new(rel);
                // Entries are written manually (not via unpack) so a
                // hostile or malformed archive cannot escape `dest`.
                if rel_path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) {
                    return Err(WizardError::IoError {
                        path: archive_c.clone(),
                        reason: format!("tar entry escapes the destination: {path_str}"),
                    });
                }
                let target = dest_c.join(rel_path);
                if entry.header().entry_type().is_dir() {
                    std::fs::create_dir_all(&target).map_err(|e| Self::io_err(&target, e))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Self::io_err(parent, e))?;
                }
                let mut bytes = Vec::new();
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| Self::io_err(&archive_c, e))?;
                std::fs::write(&target, &bytes).map_err(|e| Self::io_err(&target, e))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn extract_zip_prefixes(
        &self,
        archive: &Path,
        dest: &Path,
        prefixes: &[&str],
        strip_prefix: &str,
    ) -> Result<(), WizardError> {
        let archive_c = archive.to_path_buf();
        let dest_c = dest.to_path_buf();
        let prefixes_c: Vec<String> = prefixes.iter().map(|p| p.to_string()).collect();
        let strip_c = strip_prefix.to_string();
        let cancel = self.cancel_flag();
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| Self::io_err(&archive_c, e))?;
            for i in 0..zip.len() {
                if cancel.load(Ordering::SeqCst) {
                    return Err(WizardError::Cancelled);
                }
                let mut entry = zip.by_index(i).map_err(|e| Self::io_err(&archive_c, e))?;
                let name = entry.name().replace('\\', "/");
                if !prefixes_c.iter().any(|p| name.starts_with(p.as_str())) {
                    continue;
                }
                let Some(rel) = name.strip_prefix(strip_c.as_str()) else {
                    continue;
                };
                if rel.is_empty() {
                    continue;
                }
                let rel_path = Path::new(rel);
                // Entries are written manually (not via extract) so
                // a hostile or malformed archive cannot escape `dest`.
                if rel_path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) {
                    return Err(WizardError::IoError {
                        path: archive_c.clone(),
                        reason: format!("zip entry escapes the destination: {name}"),
                    });
                }
                let target = dest_c.join(rel_path);
                if entry.is_dir() {
                    std::fs::create_dir_all(&target).map_err(|e| Self::io_err(&target, e))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Self::io_err(parent, e))?;
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| Self::io_err(&archive_c, e))?;
                std::fs::write(&target, &bytes).map_err(|e| Self::io_err(&target, e))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn run_elevated(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
        #[cfg(windows)]
        {
            run_elevated_windows(cmd)
        }
        #[cfg(not(windows))]
        {
            Err(WizardError::IoError {
                path: PathBuf::from(cmd.argv.first().cloned().unwrap_or_default()),
                reason: "elevated execution is only supported on Windows".to_string(),
            })
        }
    }

    async fn http_get(&self, url: &str) -> Result<HttpResponse, WizardError> {
        http_get_impl(url, &[]).await
    }

    async fn http_get_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, WizardError> {
        http_get_impl(url, headers).await
    }

    async fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, WizardError> {
        http_post_impl(url, body, &[]).await
    }

    async fn http_post_with_headers(
        &self,
        url: &str,
        body: &str,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, WizardError> {
        http_post_impl(url, body, headers).await
    }

    async fn http_head_content_length(&self, url: &str) -> Result<Option<u64>, WizardError> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_GET_TIMEOUT)
            .build()
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: format!("client build failed: {e}"),
            })?;
        let resp = client
            .head(url)
            .send()
            .await
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        Ok(resp.content_length())
    }

    async fn free_space_bytes(&self, path: &Path) -> Result<u64, WizardError> {
        // Shell out rather than linking winapi: the wizard already spawns
        // subprocesses for every other probe, and this keeps the dep tree
        // unchanged.
        #[cfg(target_os = "windows")]
        let probe = PlannedCommand::new(vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            format!(
                "(Get-Item '{}').PSDrive.Free",
                path.display().to_string().replace('\'', "''")
            ),
        ]);
        #[cfg(not(target_os = "windows"))]
        let probe = PlannedCommand::new(vec![
            "df".to_string(),
            "-k".to_string(),
            "--output=avail".to_string(),
            path.display().to_string(),
        ]);
        let out = self.run_command(&probe).await?;
        if !out.success() {
            return Err(WizardError::CommandFailed {
                argv: probe.argv.clone(),
                code: out.exit_code,
                stderr: out.stderr.clone(),
            });
        }
        #[cfg(target_os = "windows")]
        let bytes = out
            .stdout
            .trim()
            .parse::<u64>()
            .map_err(|e| WizardError::CommandFailed {
                argv: probe.argv.clone(),
                code: out.exit_code,
                stderr: format!("could not parse free-space output {:?}: {e}", out.stdout),
            })?;
        #[cfg(not(target_os = "windows"))]
        let bytes = out
            .stdout
            .lines()
            .last()
            .and_then(|l| l.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .ok_or_else(|| WizardError::CommandFailed {
                argv: probe.argv.clone(),
                code: out.exit_code,
                stderr: format!("could not parse df output {:?}", out.stdout),
            })?;
        Ok(bytes)
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, WizardError> {
        tokio::fs::read(path)
            .await
            .map_err(|e| Self::io_err(path, e))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), WizardError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Self::io_err(path, e))?;
            }
        }
        tokio::fs::write(path, data)
            .await
            .map_err(|e| Self::io_err(path, e))
    }

    async fn remove_file(&self, path: &Path) -> Result<(), WizardError> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Self::io_err(path, e)),
        }
    }

    async fn remove_dir_all(&self, path: &Path) -> Result<(), WizardError> {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Self::io_err(path, e)),
        }
    }

    async fn file_exists(&self, path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn file_len(&self, path: &Path) -> Option<u64> {
        tokio::fs::metadata(path)
            .await
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len())
    }

    async fn sha256_file(&self, path: &Path) -> Result<String, WizardError> {
        // Streamed hashing via the shared implementation: the
        // previous read-the-whole-file-then-hash version
        // materialized the entire GGUF in memory, and the failed
        // allocation on a multi-GiB file aborted the whole process
        // (the v0.1.31 crash on the 45.9 GiB Coder-Next file, hit
        // by the launch-time wizard audit).
        let owned = path.to_path_buf();
        match tokio::task::spawn_blocking(move || manager_models::sha256_hex(&owned)).await {
            Ok(Ok(hex)) => Ok(hex),
            Ok(Err(e)) => Err(WizardError::IoError {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }),
            Err(e) => Err(WizardError::IoError {
                path: path.to_path_buf(),
                reason: format!("hash task failed: {e}"),
            }),
        }
    }

    async fn gpu_inventory(&self) -> Result<Vec<GpuDescriptor>, WizardError> {
        // Same System32-first nvidia-smi resolution as telemetry
        // (manager_telemetry::default_smi_path): a bare name resolves
        // through PATH, which a same-named executable could shadow.
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        let rows = PlannedCommand::new(vec![
            smi.clone(),
            "--query-gpu=index,name,uuid,memory.total,compute_cap".to_string(),
            "--format=csv,noheader".to_string(),
        ]);
        let out = self
            .run_command(&rows)
            .await
            .map_err(|e| WizardError::GpuProbeFailed(format!("nvidia-smi query failed: {e}")))?;
        if !out.success() {
            return Err(WizardError::GpuProbeFailed(format!(
                "nvidia-smi exited {}: {}",
                out.exit_code, out.stderr
            )));
        }
        let header = self
            .run_command(&PlannedCommand::new(vec![smi]))
            .await
            .map_err(|e| WizardError::GpuProbeFailed(format!("nvidia-smi failed: {e}")))?;
        let driver = Self::parse_driver_version(&header.stdout);
        Ok(Self::parse_gpu_rows(&out.stdout, &driver))
    }

    async fn credential_exists(&self, target: &str) -> Result<bool, WizardError> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut cmd = tokio::process::Command::new("cmdkey");
            cmd.arg(format!("/list:{target}"));
            // CREATE_NO_WINDOW: this probe runs from a GUI process.
            cmd.creation_flags(0x08000000);
            let out = cmd.output().await.map_err(|e| WizardError::IoError {
                path: PathBuf::from("cmdkey"),
                reason: e.to_string(),
            })?;
            Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).contains(target))
        }
        #[cfg(not(windows))]
        {
            let _ = target;
            // No Credential Manager off Windows; presence checks fail closed.
            Ok(false)
        }
    }

    async fn credential_read(&self, target: &str) -> Option<String> {
        #[cfg(windows)]
        {
            cred_read_windows(target)
        }
        #[cfg(not(windows))]
        {
            let _ = target;
            // No Credential Manager off Windows.
            None
        }
    }

    async fn credential_write(&self, target: &str, secret: &str) -> Result<(), WizardError> {
        #[cfg(windows)]
        {
            cred_write_windows(target, secret).ok_or_else(|| WizardError::IoError {
                path: PathBuf::from("Credential Manager"),
                reason: format!("CredWriteW failed for target '{target}'"),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (target, secret);
            Err(WizardError::IoError {
                path: PathBuf::from("Credential Manager"),
                reason: "credential storage is only supported on Windows".to_string(),
            })
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }

    async fn vscode_cli(&self) -> Option<String> {
        #[cfg(windows)]
        {
            find_vscode_cli_windows()
        }
        #[cfg(not(windows))]
        {
            // The PATH probe in the step is the detection mechanism
            // off Windows.
            None
        }
    }
}

/// Read a registry key's (Default) string value; `None` on any
/// failure (missing key/value, non-string value).
#[cfg(windows)]
fn reg_default_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, KEY_READ, REG_EXPAND_SZ, REG_SZ,
    };
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: windows_sys::Win32::System::Registry::HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(root, subkey_w.as_ptr(), 0, KEY_READ, &mut key) };
    if rc != 0 {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let mut ty: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr() as *mut u8,
            &mut len,
        )
    };
    unsafe { RegCloseKey(key) };
    if rc != 0 || (ty != REG_SZ && ty != REG_EXPAND_SZ) {
        return None;
    }
    buf.truncate((len as usize / 2).min(buf.len()));
    let s = String::from_utf16_lossy(&buf);
    let s = s.trim().trim_matches('"').trim_end_matches('\0').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Locate the VS Code CLI without relying on this process's PATH.
///
/// A GUI process inherits the environment of whatever launched it,
/// which may predate the VS Code install (or the PATH edit its
/// installer made), so spawning bare `code` can fail on a machine
/// where typing `code` in PowerShell works. VS Code's installer
/// registers the editor under the App Paths registry key — the
/// Windows mechanism for exactly this lookup — and the CLI lives in
/// the `bin` directory of the install root. Standard install
/// locations (system and per-user installers, stable and Insiders)
/// are the fallback.
#[cfg(windows)]
fn find_vscode_cli_windows() -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Some(exe) = reg_default_string(
            root,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Code.exe",
        ) {
            if let Some(install_dir) = PathBuf::from(exe).parent() {
                let cli = install_dir.join("bin").join("code.cmd");
                if cli.exists() {
                    return Some(cli.display().to_string());
                }
            }
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(pf));
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(la).join("Programs"));
    }
    for root in &roots {
        for (dir, cli_name) in [
            ("Microsoft VS Code", "code.cmd"),
            ("Microsoft VS Code Insiders", "code-insiders.cmd"),
        ] {
            let cli = root.join(dir).join("bin").join(cli_name);
            if cli.exists() {
                return Some(cli.display().to_string());
            }
        }
    }
    None
}

/// Read a generic credential's blob via CredReadW (advapi32). Returns
/// `None` on any failure (missing target, empty blob, undecodable
/// content). The returned value is a secret: it is never logged and
/// never written to disk by callers — it goes straight into a spawned
/// process's environment.
#[cfg(windows)]
fn cred_read_windows(target: &str) -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    let ok = unsafe { CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
    if ok == 0 || cred.is_null() {
        return None;
    }
    let value = unsafe {
        let size = (*cred).CredentialBlobSize as usize;
        let ptr = (*cred).CredentialBlob;
        if ptr.is_null() || size == 0 {
            None
        } else {
            decode_credential_blob(std::slice::from_raw_parts(ptr, size))
        }
    };
    unsafe { CredFree(cred as *const std::ffi::c_void) };
    value.filter(|v| !v.is_empty())
}

/// Store a generic credential via CredWriteW (advapi32), replacing
/// any existing value for the target. The blob is written as
/// UTF-16LE text — the encoding cmdkey / PowerShell-written secrets
/// use and the one [`cred_read_windows`] decodes natively. Persisted
/// per-user on this machine (CRED_PERSIST_LOCAL_MACHINE).
#[cfg(windows)]
fn cred_write_windows(target: &str, secret: &str) -> Option<()> {
    use windows_sys::Win32::Security::Credentials::{
        CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };
    let mut target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let blob: Vec<u8> = secret
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let mut cred: CREDENTIALW = unsafe { std::mem::zeroed() };
    cred.Type = CRED_TYPE_GENERIC;
    cred.TargetName = target_wide.as_mut_ptr();
    cred.CredentialBlobSize = blob.len() as u32;
    cred.CredentialBlob = blob.as_ptr() as *mut u8;
    cred.Persist = CRED_PERSIST_LOCAL_MACHINE;
    let ok = unsafe { CredWriteW(&cred, 0) };
    if ok == 0 {
        None
    } else {
        Some(())
    }
}

/// Decode a credential blob: generic credentials written by cmdkey /
/// PowerShell hold the secret as UTF-16LE text; anything else is read as
/// UTF-8. Trailing NULs are trimmed either way.
///
/// The UTF-16LE reading is accepted outright only when every decoded
/// char is ASCII: `String::from_utf16` succeeds for most arbitrary byte
/// pairs, so an even-length UTF-8 blob would otherwise "decode" into
/// garbage codepoints and be mistaken for the secret. When the UTF-16
/// reading is not plain ASCII, a valid UTF-8 reading wins; the UTF-16
/// reading remains the fallback (genuine UTF-16LE secrets containing
/// non-ASCII chars are not valid UTF-8 as bytes, so they land there).
#[cfg(any(windows, test))]
fn decode_credential_blob(blob: &[u8]) -> Option<String> {
    let utf16 = if blob.len() >= 2 && blob.len().is_multiple_of(2) {
        let units: Vec<u16> = blob
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes(*p))
            .collect();
        String::from_utf16(&units)
            .ok()
            .map(|s| s.trim_end_matches('\0').to_string())
    } else {
        None
    };
    if let Some(s) = &utf16 {
        if !s.is_empty() && s.is_ascii() {
            return Some(s.clone());
        }
    }
    if let Ok(s) = std::str::from_utf8(blob) {
        let s = s.trim_end_matches('\0').to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Some(s) = utf16 {
        if !s.is_empty() {
            return Some(s);
        }
    }
    let s = String::from_utf8_lossy(blob)
        .trim_end_matches('\0')
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gpu_rows_handles_realistic_output() {
        let text = "0, NVIDIA RTX A4000, GPU-aaaa, 16384 MiB, 8.6\n\
                    1, Tesla P100-PCIE-16GB, GPU-bbbb, 16384 MiB, 6.0\n";
        let gpus = RealSystemOps::parse_gpu_rows(text, "581.57");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA RTX A4000");
        assert_eq!(gpus[0].compute_capability, (8, 6));
        assert!(!gpus[0].is_pascal_or_older());
        assert_eq!(gpus[1].compute_capability, (6, 0));
        assert!(gpus[1].is_pascal_or_older());
        assert_eq!(gpus[1].total_vram_mib, 16384);
        assert_eq!(gpus[1].driver_version, "581.57");
    }

    #[test]
    fn parse_driver_version_from_header() {
        let header = "| NVIDIA-SMI 581.57                 Driver Version: 581.57         CUDA Version: 12.9     |";
        assert_eq!(RealSystemOps::parse_driver_version(header), "581.57");
        assert_eq!(RealSystemOps::parse_driver_version("nope"), "0.0");
    }

    #[tokio::test]
    async fn run_command_empty_argv_errors() {
        let ops = RealSystemOps::default();
        let err = ops
            .run_command(&PlannedCommand::new(vec![]))
            .await
            .unwrap_err();
        assert!(matches!(err, WizardError::CommandFailed { .. }));
    }

    /// A stalled child must not hold the caller past the budget:
    /// `run_command_with_timeout` returns a timeout error promptly
    /// (and, via kill-on-drop in the real run_command, the child is
    /// killed rather than left running). Unix-only: `sleep` is the
    /// portable stalled child there; the Windows CI path exercises
    /// the same code through the default 600 s budget constant.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_command_with_timeout_bounds_stalled_child() {
        let ops = RealSystemOps::default();
        let cmd = PlannedCommand::new(vec!["sleep".to_string(), "30".to_string()]);
        let started = std::time::Instant::now();
        let err = ops
            .run_command_with_timeout(&cmd, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, WizardError::CommandFailed { .. }));
        assert!(format!("{err}").contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned after {:?}",
            started.elapsed()
        );
    }

    /// A timed-out command's whole TREE dies, not just the direct
    /// child (code-review 2.2/2.15): the shell records its own pid
    /// and its background child's pid, then sleeps; the caller's
    /// budget expires, and both pids must be gone afterwards.
    /// (The previous coverage asserted only that the call returned.)
    #[cfg(unix)]
    #[tokio::test]
    async fn run_command_timeout_kills_the_process_tree() {
        let ops = RealSystemOps::default();
        let dir = std::env::temp_dir().join(format!("wizard-tree-kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pids");
        let script = format!(
            "echo $$ > {}; sleep 60 & echo $! >> {}; wait",
            pid_file.display(),
            pid_file.display()
        );
        let cmd = PlannedCommand::new(vec!["sh".to_string(), "-c".to_string(), script]);
        let err = ops
            .run_command_with_timeout(&cmd, Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("timed out"), "{err}");
        let text = std::fs::read_to_string(&pid_file).unwrap();
        let pids: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
        assert_eq!(pids.len(), 2, "shell + grandchild pids recorded: {text:?}");
        // The tree kill is asynchronous to the error return; poll
        // briefly for both pids to disappear.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && pids.iter().any(|p| process_alive(*p)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for pid in &pids {
            assert!(!process_alive(*pid), "pid {pid} survived the tree kill");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cancelled step's extraction stops instead of writing the
    /// whole archive in the background (code-review 2.2).
    #[tokio::test]
    async fn extract_zip_honors_cancellation() {
        use std::io::Write as _;
        let ops = RealSystemOps::default();
        let dir =
            std::env::temp_dir().join(format!("wizard-extract-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("a.zip");
        {
            let mut zw = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            for name in ["one.txt", "two.txt", "three.txt"] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(b"payload").unwrap();
            }
            zw.finish().unwrap();
        }
        ops.request_cancellation();
        let dest = dir.join("out");
        let err = ops.extract_zip(&archive, &dest).await.unwrap_err();
        assert!(matches!(err, WizardError::Cancelled), "{err}");
        assert!(
            !dest.join("one.txt").exists(),
            "cancelled extraction wrote files"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sha256_roundtrip_on_temp_file() {
        let ops = RealSystemOps::default();
        let path = std::env::temp_dir().join("wizard-sha256-test.bin");
        ops.write_file(&path, b"hello").await.unwrap();
        let hex = ops.sha256_file(&path).await.unwrap();
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(ops.file_exists(&path).await);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn sha256_streams_multi_chunk_file() {
        // Regression for the v0.1.31 crash: sha256_file must stream,
        // not materialize the file. A patterned file spanning
        // several hash buffers, checked against an independently
        // computed digest (the digest itself is size-independent;
        // the >RAM case is covered by manager-models' ignored
        // multi-GiB test for the shared implementation).
        use sha2::{Digest, Sha256};
        let ops = RealSystemOps::default();
        let path = std::env::temp_dir().join("wizard-sha256-stream-test.bin");
        let total = 3 * (1usize << 20) + 17;
        let pattern: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        ops.write_file(&path, &pattern).await.unwrap();
        let mut reference = Sha256::new();
        reference.update(&pattern);
        let expected = format!("{:x}", reference.finalize());
        assert_eq!(ops.sha256_file(&path).await.unwrap(), expected);
        std::fs::remove_file(&path).ok();
    }

    /// Regression (S6): `spawn_detached` must deliver `PlannedCommand.env`
    /// to the child — the gateway's OpenRouter key travels exclusively
    /// that way, and the fake records env verbatim so only a real-spawn
    /// test can catch the drop.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_detached_delivers_env_to_child() {
        let ops = RealSystemOps::default();
        let out_path =
            std::env::temp_dir().join(format!("wizard-spawn-env-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out_path);
        let mut cmd = PlannedCommand::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("printf %s \"$WAVE3_TEST_VAR\" > {}", out_path.display()),
        ]);
        cmd.env
            .insert("WAVE3_TEST_VAR".to_string(), "env-delivered".to_string());
        let pid = ops.spawn_detached(&cmd).await.unwrap();
        assert!(pid > 0);
        let mut contents = String::new();
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&out_path) {
                contents = s;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = std::fs::remove_file(&out_path);
        assert_eq!(contents, "env-delivered");
    }

    /// A detached spawn with log files named captures the child's
    /// stdout and stderr to them, and a second spawn rotates the
    /// first run's files to `.prev`.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_detached_captures_output_to_log_files() {
        let ops = RealSystemOps::default();
        let dir = std::env::temp_dir().join(format!("wizard-spawn-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out_log = dir.join("gateway.stdout.log");
        let err_log = dir.join("gateway.stderr.log");
        let spawn_once = |marker: &str| {
            let mut cmd = PlannedCommand::new(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo out-{marker}; echo err-{marker} >&2"),
            ]);
            cmd.stdout_log = Some(out_log.clone());
            cmd.stderr_log = Some(err_log.clone());
            cmd
        };
        let read_when = async |path: &std::path::Path, needle: &str| {
            for _ in 0..50 {
                if let Ok(s) = std::fs::read_to_string(path) {
                    if s.contains(needle) {
                        return s;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            String::new()
        };
        ops.spawn_detached(&spawn_once("one")).await.unwrap();
        assert!(read_when(&out_log, "out-one").await.contains("out-one"));
        assert!(read_when(&err_log, "err-one").await.contains("err-one"));
        // Second spawn: run one's logs rotate to .prev.
        ops.spawn_detached(&spawn_once("two")).await.unwrap();
        assert!(read_when(&out_log, "out-two").await.contains("out-two"));
        let prev = prev_log_path(&out_log);
        assert!(
            std::fs::read_to_string(&prev)
                .unwrap_or_default()
                .contains("out-one"),
            "previous run kept in {}",
            prev.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_credential_blob_reads_utf16le_ascii() {
        let blob: Vec<u8> = "s3cret"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_credential_blob(&blob).as_deref(), Some("s3cret"));
    }

    #[test]
    fn decode_credential_blob_reads_even_length_utf8() {
        // An 8-byte ASCII blob: the previous UTF-16LE-first heuristic
        // "decoded" this into garbage codepoints and returned those.
        assert_eq!(
            decode_credential_blob(b"hunter22").as_deref(),
            Some("hunter22")
        );
    }

    #[test]
    fn decode_credential_blob_reads_utf16le_non_ascii() {
        let blob: Vec<u8> = "pässwörd"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_credential_blob(&blob).as_deref(), Some("pässwörd"));
    }

    #[test]
    fn decode_credential_blob_edge_cases() {
        // Odd length: no UTF-16 reading is attempted.
        assert_eq!(decode_credential_blob(b"abc").as_deref(), Some("abc"));
        assert_eq!(decode_credential_blob(b""), None);
        assert_eq!(decode_credential_blob(&[0, 0]), None);
    }

    /// `kill_process` terminates a real spawned process (bare-pid
    /// SIGTERM on unix; the recorded gateway pid is the server itself).
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_terminates_spawned_process() {
        let ops = RealSystemOps::default();
        let cmd = PlannedCommand::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ]);
        let pid = ops.spawn_detached(&cmd).await.unwrap();
        assert!(pid > 0 && process_alive(pid));
        kill_process(pid).unwrap();
        let mut alive = true;
        for _ in 0..50 {
            if !process_alive(pid) {
                alive = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!alive, "spawned process was not terminated");
    }

    #[test]
    fn stop_gateway_without_pid_file_is_false() {
        let dir = std::env::temp_dir().join(format!("wizard-stop-gw-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(stop_gateway(&dir), Ok(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stop_gateway_with_stale_pid_cleans_up() {
        let dir = std::env::temp_dir().join(format!("wizard-stop-gw-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A pid guaranteed dead: a child that already exited and was
        // reaped. (A merely huge number would be a dangerous stand-in:
        // values above i32::MAX wrap to negative pid_t values, where
        // -1 signals every process the user owns.)
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(gateway_pid_path(&dir), dead_pid.to_string()).unwrap();
        assert_eq!(stop_gateway(&dir), Ok(false));
        assert!(!gateway_pid_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_pids_are_never_signalled() {
        // 0 = own process group; i32::MAX+1.. wraps to negative pid_t
        // (-1 = every process the user owns). Both must be refused
        // before any syscall.
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
        assert!(kill_process(0).is_err());
        assert!(kill_process(u32::MAX).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stop_gateway_kills_recorded_process() {
        let dir = std::env::temp_dir().join(format!("wizard-stop-gw-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        std::fs::write(gateway_pid_path(&dir), pid.to_string()).unwrap();
        assert!(process_alive(pid));
        assert_eq!(stop_gateway(&dir), Ok(true));
        assert!(!gateway_pid_path(&dir).exists());
        let mut exited = false;
        for _ in 0..50 {
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(exited, "recorded process was not terminated");
    }
}
