//! Child-process execution: budgeted `run_child`, pipe readers, and creation flags.

use super::*;

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
    pub(crate) async fn run_child(
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
            // CREATE_NO_WINDOW. `c` is a tokio::process::Command,
            // whose creation_flags is an inherent method on Windows —
            // no std CommandExt import is needed (or used) here.
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
        let mut child = c.spawn().map_err(|e| WizardError::CommandFailed {
            argv: cmd.argv.clone(),
            code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
        let mut guard = TreeKillGuard {
            pid: child.id().filter(|p| pid_is_signalable(*p)),
        };
        // Wait for the CHILD to exit, not for its pipes to close
        // (finding PL-M3, docs/code-review-v0.1.60.md).
        // `wait_with_output` waited for EOF on stdout/stderr, and a
        // descendant that inherited the pipe write ends keeps them open
        // for its whole life — `pg_ctl --no-wait start` hands them to
        // `cmd /C postgres` and the postmaster, so every PostgreSQL
        // start sat out its full 60 s budget and was then logged as a
        // timed-out launch. The pipes are drained on reader tasks; once
        // the child has exited they get a short grace period, and output
        // still held open by a descendant is returned as read so far.
        let stdout_buf = spawn_pipe_reader(child.stdout.take());
        let stderr_buf = spawn_pipe_reader(child.stderr.take());
        let status = match tokio::time::timeout(budget, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                stdout_buf.1.abort();
                stderr_buf.1.abort();
                return Err(WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("wait failed: {e}"),
                });
            }
            Err(_) => {
                // The armed guard kills the tree when it drops.
                stdout_buf.1.abort();
                stderr_buf.1.abort();
                return Err(WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("timed out after {}s and was stopped", budget.as_secs()),
                });
            }
        };
        // The child exited: its descendants (a started server) are
        // meant to outlive it, so the tree is not killed.
        guard.disarm();
        let stdout = finish_pipe_reader(stdout_buf).await;
        let stderr = finish_pipe_reader(stderr_buf).await;
        Ok(CommandOutput {
            exit_code: status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// The step-cancellation flag, cloned into blocking work (the
    /// extractors run on `spawn_blocking` threads where `self` is
    /// unavailable): they check it per entry and fail with
    /// `WizardError::Cancelled`.
    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.step_cancel.clone()
    }
}

/// How long [`RealSystemOps::run_child`] keeps reading a child's pipes
/// after the child itself has exited. Output a command writes before
/// exiting is already in the pipe; this only bounds the wait for EOF
/// when a descendant inherited the write end (PL-M3).
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

type PipeReader = (Arc<std::sync::Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>);

/// Read `pipe` to EOF on a task, appending to a shared buffer as data
/// arrives so an aborted read keeps everything read so far.
fn spawn_pipe_reader<R>(pipe: Option<R>) -> PipeReader
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = buf.clone();
    let task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sink
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .extend_from_slice(&chunk[..n]),
            }
        }
    });
    (buf, task)
}

/// Give a pipe reader [`PIPE_DRAIN_GRACE`] to reach EOF, then stop it
/// and return what it read.
async fn finish_pipe_reader((buf, mut task): PipeReader) -> Vec<u8> {
    if tokio::time::timeout(PIPE_DRAIN_GRACE, &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
    let out = std::mem::take(&mut *buf.lock().unwrap_or_else(|p| p.into_inner()));
    out
}

/// Windows creation flags for a windowless detached spawn:
/// CREATE_NO_WINDOW alone. DETACHED_PROCESS must NOT be combined
/// with it: Windows ignores CREATE_NO_WINDOW when combined with
/// DETACHED_PROCESS, leaving a console-less child whose console
/// children each open a visible window (the gateway's Prisma query
/// engine window, 2026-11-02). A named helper so a test pins the
/// combination — the flags themselves are not observable from
/// outside the spawned process.
#[cfg(windows)]
pub(crate) fn windowless_creation_flags() -> u32 {
    0x08000000
}
