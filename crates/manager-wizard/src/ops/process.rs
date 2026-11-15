//! Process liveness, gateway pid resolution, stopping the gateway, and process-tree kill.

use super::*;

/// A pid is only safe to signal when it names one specific process:
/// 0 means "my own process group" and a value above `i32::MAX` wraps
/// to a negative `pid_t` — -1 is "every process I own". A corrupt or
/// hand-edited pid file must never turn a liveness check or a stop
/// into a broadcast signal, so out-of-range pids are refused here,
/// at the only place pids reach the OS.
pub(crate) fn pid_is_signalable(pid: u32) -> bool {
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

/// Whether the process with `pid` is an instance of the named
/// executable image (`"postgres.exe"`), when the platform can say.
///
/// Windows: `tasklist` in CSV form filtered to the pid; the first
/// field is the image name (same shell-out approach as
/// [`process_alive`] — no extra FFI). Unix: `ps -p <pid> -o comm=`.
/// A probe that cannot be carried out returns `None` (unknown);
/// a probe that runs and finds no such process returns
/// `Some(false)` — the pid names nothing, so it is certainly not
/// the named image.
pub fn process_image_is(pid: u32, image: &str) -> Option<bool> {
    if !pid_is_signalable(pid) {
        return Some(false);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        // CREATE_NO_WINDOW: this probe runs from a GUI process.
        cmd.creation_flags(0x08000000);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let first = text.lines().next()?.trim();
        // A live process renders as `"image.exe","pid",...`; no
        // match renders as an INFO line with no quoted image field.
        let found = first
            .strip_prefix('"')
            .and_then(|s| s.split('"').next())
            .unwrap_or("");
        Some(found.eq_ignore_ascii_case(image))
    }
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        if !out.status.success() {
            return Some(false);
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let base = name.rsplit('/').next().unwrap_or(&name);
        Some(base.eq_ignore_ascii_case(image))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = image;
        None
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

/// Executable image of the gateway process recorded in `gateway.pid`
/// on Windows (the venv's `Scripts\python.exe`).
pub const GATEWAY_PROCESS_IMAGE: &str = "python.exe";

/// Whether `pid` is a gateway interpreter (finding PL-M4,
/// `docs/code-review-v0.1.60.md`): its image is `python.exe` on
/// Windows. `Some(false)` means a live process of another image — the
/// recorded pid was recycled (after a reboot, say) and must not be
/// killed with `taskkill /T /F`; `None` means the platform cannot tell
/// (always off Windows, where the gateway is a development path only
/// and a dev interpreter's command name varies), and callers proceed
/// as before.
pub fn process_is_gateway(pid: u32) -> Option<bool> {
    #[cfg(windows)]
    {
        process_image_is(pid, GATEWAY_PROCESS_IMAGE)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// How a gateway process was found (see [`resolve_gateway_pid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FoundBy {
    /// Recorded in `gateway.pid`.
    PidFile,
    /// No usable pid file; the gateway interpreter owning the
    /// gateway port's LISTEN socket.
    PortOwner,
    /// Nothing found.
    None,
}

impl FoundBy {
    /// Marker/log spelling ("pid file", "port owner", "none").
    pub fn label(self) -> &'static str {
        match self {
            FoundBy::PidFile => "pid file",
            FoundBy::PortOwner => "port owner",
            FoundBy::None => "none",
        }
    }
}

/// Result of [`stop_gateway_and_wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GatewayStopOutcome {
    /// A live gateway process was terminated and confirmed gone.
    pub stopped: bool,
    /// The pid that was stopped, when one was found.
    pub pid: Option<u32>,
    /// How the pid was found.
    pub found_by: FoundBy,
    /// The graceful phase alone ended the process (no forced kill).
    pub graceful: bool,
    /// The gateway port had no listener when the stop returned
    /// (`true` when no port was given).
    pub port_released: bool,
}

/// The live gateway process, if any: the pid recorded in
/// `<data_dir>/gateway.pid` when it is alive and not a recycled pid
/// of another program (`process_is_gateway != Some(false)`); else the
/// owner of the gateway port's LISTEN socket when it is positively a
/// gateway interpreter (`process_is_gateway == Some(true)` — an
/// unknown image is never adopted by port, the same rule backend
/// adoption uses). `port == 0` skips the port lookup.
pub fn resolve_gateway_pid(data_dir: &Path, port: u16) -> Option<(u32, FoundBy)> {
    let recorded = std::fs::read_to_string(gateway_pid_path(data_dir))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok());
    let port_owner = if port == 0 {
        None
    } else {
        manager_supervisor::port_owner_pid(port)
    };
    pick_gateway_pid(recorded, port_owner, process_alive, process_is_gateway)
}

/// The decision behind [`resolve_gateway_pid`] (and the start-gateway
/// step's ops-based twin), over injected probes so it is unit-testable.
pub fn pick_gateway_pid(
    recorded: Option<u32>,
    port_owner: Option<u32>,
    alive: impl Fn(u32) -> bool,
    is_gateway: impl Fn(u32) -> Option<bool>,
) -> Option<(u32, FoundBy)> {
    if let Some(pid) = recorded {
        if alive(pid) && is_gateway(pid) != Some(false) {
            return Some((pid, FoundBy::PidFile));
        }
    }
    let pid = port_owner?;
    (alive(pid) && is_gateway(pid) == Some(true)).then_some((pid, FoundBy::PortOwner))
}

/// Ask `pid` (and its tree) to exit without forcing it. Windows:
/// `taskkill /PID <pid> /T` without `/F` (posts WM_CLOSE; a windowless
/// console interpreter may refuse, which the caller treats as "no
/// graceful phase"). Unix: SIGTERM. Returns whether the request was
/// delivered.
fn request_graceful_exit(pid: u32) -> bool {
    if !pid_is_signalable(pid) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T"]);
        // CREATE_NO_WINDOW: this runs from a GUI process.
        cmd.creation_flags(0x08000000);
        cmd.output().map(|o| o.status.success()).unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::process::Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
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

/// Poll `done` every 100 ms for up to `budget`; true once it holds.
fn wait_until(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = std::time::Instant::now();
    loop {
        if done() {
            return true;
        }
        if started.elapsed() >= budget {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Graceful-phase budget of [`stop_gateway_and_wait`].
pub const GATEWAY_GRACEFUL_STOP: Duration = Duration::from_secs(3);
/// Exit-confirmation budget of [`stop_gateway_and_wait`] after the
/// forced kill (and for the port to be released).
pub const GATEWAY_STOP_CONFIRM: Duration = Duration::from_secs(10);

/// Stop the process-mode gateway and wait until it is really gone.
///
/// 1. Find it with [`resolve_gateway_pid`] (pid file, else the port
///    owner): a lost or stale pid file no longer reads as "not
///    running" while a gateway still serves (CR-4).
/// 2. `graceful`: ask it to exit ([`request_graceful_exit`]) and wait
///    up to [`GATEWAY_GRACEFUL_STOP`] (GW-9).
/// 3. Otherwise / then: [`kill_process`] (`taskkill /T /F`).
/// 4. Wait up to [`GATEWAY_STOP_CONFIRM`] for the pid to exit, then
///    for the port to have no listener.
///
/// The pid file is removed once nothing gateway is running; it is
/// kept when the kill fails or the process outlives the wait, so a
/// retry knows the pid. Blocking: run it off the async runtime.
/// `port == 0` skips the port lookup and the port wait.
pub fn stop_gateway_and_wait(
    data_dir: &Path,
    port: u16,
    graceful: bool,
) -> Result<GatewayStopOutcome, String> {
    let pid_path = gateway_pid_path(data_dir);
    let port_free = || port == 0 || manager_supervisor::port_owner_pid(port).is_none();
    let Some((pid, found_by)) = resolve_gateway_pid(data_dir, port) else {
        let _ = std::fs::remove_file(&pid_path);
        return Ok(GatewayStopOutcome {
            stopped: false,
            pid: None,
            found_by: FoundBy::None,
            graceful: false,
            port_released: port_free(),
        });
    };
    let mut ended_gracefully = false;
    if graceful && request_graceful_exit(pid) {
        ended_gracefully = wait_until(GATEWAY_GRACEFUL_STOP, || !process_alive(pid));
    }
    if !ended_gracefully {
        if let Err(e) = kill_process(pid) {
            // taskkill also fails on a process that exited meanwhile.
            if process_alive(pid) {
                return Err(format!("could not stop gateway (pid {pid}): {e}"));
            }
        }
        if !wait_until(GATEWAY_STOP_CONFIRM, || !process_alive(pid)) {
            return Err(format!(
                "gateway (pid {pid}) is still running {} s after it was killed",
                GATEWAY_STOP_CONFIRM.as_secs()
            ));
        }
    }
    let port_released = wait_until(GATEWAY_STOP_CONFIRM, port_free);
    let _ = std::fs::remove_file(&pid_path);
    Ok(GatewayStopOutcome {
        stopped: true,
        pid: Some(pid),
        found_by,
        graceful: ended_gracefully,
        port_released,
    })
}

/// Stop the gateway process recorded in `<data_dir>/gateway.pid`.
///
/// Compatibility wrapper for existing callers: pid file only, no exit
/// wait. New code uses [`stop_gateway_and_wait`], which also finds an
/// unrecorded gateway by its port and confirms the exit.
///
/// Returns `Ok(true)` when a live recorded process was terminated,
/// `Ok(false)` when there was nothing to stop (no pid file, an
/// unparsable pid, the process already gone, or a live pid that is not
/// a gateway interpreter — a recycled pid, never killed). The pid file
/// is removed in those cases; it is kept only when the kill itself
/// fails, so a retry still knows which pid to target.
pub fn stop_gateway(data_dir: &Path) -> Result<bool, String> {
    let pid_path = gateway_pid_path(data_dir);
    let text = match std::fs::read_to_string(&pid_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("could not read {}: {e}", pid_path.display())),
    };
    let pid: Option<u32> = text.trim().parse().ok();
    let stopped = match pid {
        // `taskkill /T /F` on a recycled pid would take down an
        // unrelated program and its whole tree (PL-M4).
        Some(pid) if process_alive(pid) && process_is_gateway(pid) != Some(false) => {
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
pub(crate) fn kill_process_tree(pid: u32) {
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
pub(crate) struct TreeKillGuard {
    pub(crate) pid: Option<u32>,
}

impl TreeKillGuard {
    pub(crate) fn disarm(&mut self) {
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
