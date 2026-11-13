//! Shared step plumbing: progress logging, command helpers, and version parsing.

use super::*;

// ---------------------------------------------------------------------------
// Step progress logging
// ---------------------------------------------------------------------------
//
// A step's collected log (the `Vec<String>` every step builds) is
// persisted by the caller only when the step RETURNS, so on its own
// it leaves the wizard log silent for the whole run — a cold
// multi-minute install reads as a hang (field report 2026-10-19).
// The helpers here pair the collected log with the context's live
// sink: phase boundaries and verdicts go to both ([`push_log`]);
// high-frequency progress (download percentages, wait ticks) goes
// to the sink only ([`WizardContext::emit_log`]), so the collected
// log stays a readable end-of-step record instead of a transcript
// of every tick.

/// Record one step log line: appended to the step's collected log
/// and emitted to the live sink at the same moment.
pub(crate) fn push_log(ctx: &WizardContext, log: &mut Vec<String>, line: String) {
    ctx.emit_log(&line);
    log.push(line);
}

/// Write a gateway lifecycle marker (`logs/gateway.lifecycle.log`,
/// the gateway's own log scope).
pub(crate) fn gateway_mark(ctx: &WizardContext, event: &str, detail: Option<&str>) {
    crate::mark(
        ctx.ops.as_ref(),
        &crate::gateway_lifecycle_log(&ctx.data_dir),
        "gateway",
        event,
        detail,
    );
}

/// Write a PostgreSQL lifecycle marker (`logs/postgres.lifecycle.log`).
pub(crate) fn postgres_mark(
    ops: &dyn SystemOps,
    data_dir: &Path,
    event: &str,
    detail: Option<&str>,
) {
    crate::mark(
        ops,
        &crate::postgres_lifecycle_log(data_dir),
        "postgres",
        event,
        detail,
    );
}

/// The first non-empty line of a (possibly multi-line) message — a
/// failure reason for a one-line marker, without the log tail that
/// follows it.
pub(crate) fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Compact elapsed label for start/finish pairs ("83.2s", "2m 05s").
pub(crate) fn fmt_elapsed(start: std::time::Instant) -> String {
    let secs = start.elapsed().as_secs_f64();
    if secs < 120.0 {
        format!("{secs:.1}s")
    } else {
        let total = secs.round() as u64;
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

/// The last `max_lines` lines of a captured output, for attaching a
/// failing command's own words to the step log without dumping the
/// whole capture.
pub(crate) fn output_tail_lines(text: &str, max_lines: usize) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(max_lines)..].to_vec()
}

/// One-line failure evidence for a probe: exit code plus the tail
/// of whichever capture carries text (a Python probe's traceback
/// lands on stderr; a version print lands on stdout). Probes used
/// to discard this and report a bare bool/None, which threw away
/// exactly the line that says WHY a healthy-looking install failed
/// verification — the failure then looked identical to a missing
/// install and triggered the full repair path blind.
pub(crate) fn probe_failure_detail(out: &CommandOutput) -> String {
    let capture = if !out.stderr.trim().is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    let tail = output_tail_lines(capture.trim(), 3).join(" | ");
    if tail.is_empty() {
        format!("exit {} (no output)", out.exit_code)
    } else {
        format!("exit {}: {tail}", out.exit_code)
    }
}

/// Extract a `major.minor` version from a probe's stdout: the LAST
/// line that is exactly a two-part version. Whole-stdout equality
/// (the previous parse) treated any extra output — an interpreter
/// warning, a sitecustomize banner — as a different version, which
/// read as "wrong Python" and forced a full venv rebuild on every
/// start. Noise around the version is not a different version.
pub(crate) fn parse_two_part_version(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let line = line.trim();
        let mut parts = line.split('.');
        let major = parts.next()?;
        let minor = parts.next()?;
        if parts.next().is_some()
            || major.is_empty()
            || minor.is_empty()
            || !major.bytes().all(|b| b.is_ascii_digit())
            || !minor.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some(line.to_string())
    })
}

/// Extract a `major.minor.patch` version from a probe's stdout:
/// the LAST line that starts with three numeric components (a
/// pre-release/build suffix is allowed). See
/// [`parse_two_part_version`] for why the parse tolerates noise.
pub(crate) fn parse_semver_version(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let line = line.trim();
        let mut numeric = 0usize;
        let mut end = 0usize;
        for (i, c) in line.char_indices() {
            if c.is_ascii_digit() {
                end = i + c.len_utf8();
            } else if c == '.' && numeric < 3 {
                numeric += 1;
                end = i + 1;
            } else {
                break;
            }
        }
        // Count the components in the consumed digit/dot prefix.
        let prefix = &line[..end];
        let components = prefix.split('.').filter(|p| !p.is_empty()).count();
        if components == 3 && prefix.bytes().filter(|&b| b == b'.').count() == 2 {
            // The character after the patch digits must not extend
            // the number (a fourth component is a different shape).
            match line[end..].chars().next() {
                None => Some(line.to_string()),
                Some(c) if !c.is_ascii_digit() && c != '.' => Some(line.to_string()),
                _ => None,
            }
        } else {
            None
        }
    })
}

/// Run a command with the step log wrapped around it: the argv is
/// recorded BEFORE the run (so a long command is never a silent
/// window), the exit and elapsed time after it, and on a non-zero
/// exit the tail of the captured output joins the log — a failed
/// command's own words belong in the step log, not only in the
/// failure message. A spawn/timeout error is logged the same way
/// and returned to the caller.
pub(crate) async fn run_command_logged(
    ctx: &WizardContext,
    log: &mut Vec<String>,
    cmd: &PlannedCommand,
    timeout: Option<std::time::Duration>,
) -> Result<CommandOutput, WizardError> {
    push_log(ctx, log, format!("running: {}", cmd.argv.join(" ")));
    let started = std::time::Instant::now();
    let result = match timeout {
        Some(budget) => ctx.ops.run_command_with_timeout(cmd, budget).await,
        None => ctx.ops.run_command(cmd).await,
    };
    match result {
        Ok(out) => {
            push_log(
                ctx,
                log,
                format!("exit {} in {}", out.exit_code, fmt_elapsed(started)),
            );
            if !out.success() {
                let (label, capture) = if !out.stderr.trim().is_empty() {
                    ("stderr", &out.stderr)
                } else {
                    ("stdout", &out.stdout)
                };
                let tail = output_tail_lines(capture.trim(), 10);
                if !tail.is_empty() {
                    push_log(ctx, log, format!("command {label} (last lines):"));
                    for line in tail {
                        push_log(ctx, log, line.to_string());
                    }
                }
            }
            Ok(out)
        }
        Err(e) => {
            push_log(
                ctx,
                log,
                format!("command failed in {}: {e}", fmt_elapsed(started)),
            );
            Err(e)
        }
    }
}

/// Throttled download progress for the live sink: the first call,
/// every ~10-point percentage step, at least every 5 s while bytes
/// move, and completion. The collected step log keeps only the
/// download's start/finish lines (recorded by the caller) — the
/// percentage ticks are live-only progress, not record.
pub(crate) struct ProgressEmitter {
    sink: Option<StepLogSink>,
    label: String,
    started: std::time::Instant,
    last_emit_ms: std::sync::atomic::AtomicU64,
    last_pct: std::sync::atomic::AtomicU64,
}

impl ProgressEmitter {
    pub(crate) fn new(ctx: &WizardContext, label: impl Into<String>) -> Self {
        Self {
            sink: ctx.log_sink(),
            label: label.into(),
            started: std::time::Instant::now(),
            last_emit_ms: std::sync::atomic::AtomicU64::new(u64::MAX),
            // u64::MAX = no percentage emitted yet.
            last_pct: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }

    pub(crate) fn progress(&self, downloaded: u64, total: u64) {
        use std::sync::atomic::Ordering;
        let Some(sink) = &self.sink else {
            return;
        };
        let pct = downloaded.saturating_mul(100).checked_div(total);
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let last_ms = self.last_emit_ms.load(Ordering::Relaxed);
        let first = last_ms == u64::MAX;
        let pct_step = match (pct, self.last_pct.load(Ordering::Relaxed)) {
            (Some(p), prev) => prev == u64::MAX || p >= prev + 10,
            (None, _) => false,
        };
        let complete = total > 0 && downloaded >= total;
        let due = first || pct_step || complete || elapsed_ms >= last_ms + 5_000;
        if !due {
            return;
        }
        self.last_emit_ms.store(elapsed_ms, Ordering::Relaxed);
        if let Some(p) = pct {
            self.last_pct.store(p, Ordering::Relaxed);
        }
        sink(match pct {
            Some(p) => format!(
                "downloading {}: {downloaded} / {total} bytes ({p}%)",
                self.label
            ),
            None => format!("downloading {}: {downloaded} bytes", self.label),
        });
    }
}

/// Incremental reader over an append-only log file (the
/// postmaster's own log): each call returns the lines written since
/// the previous call, so a wait loop can stream a starting
/// server's FATAL lines into the step log as they land instead of
/// only attaching a tail after the wait has already failed.
pub(crate) struct FileLineStreamer {
    emitted: usize,
}

impl FileLineStreamer {
    pub(crate) fn new() -> Self {
        Self { emitted: 0 }
    }

    /// Start from the file's current end: lines already in the
    /// file predate this wait and are not this wait's news.
    pub(crate) async fn prime(&mut self, ops: &dyn SystemOps, path: &Path) {
        self.emitted = match ops.read_file(path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).lines().count(),
            Err(_) => 0,
        };
    }

    /// Emit every line appended since the last call, prefixed so
    /// the step log attributes them to their writer.
    pub(crate) async fn stream_new(
        &mut self,
        ctx: &WizardContext,
        ops: &dyn SystemOps,
        path: &Path,
        prefix: &str,
        log: &mut Vec<String>,
    ) {
        let Ok(bytes) = ops.read_file(path).await else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < self.emitted {
            // The file shrank (rotated/recreated): resync to its
            // end rather than replaying it.
            self.emitted = lines.len();
            return;
        }
        for line in &lines[self.emitted..] {
            push_log(ctx, log, format!("{prefix}{line}"));
        }
        self.emitted = lines.len();
    }
}
