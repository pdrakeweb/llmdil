//! Shared service-lifecycle vocabulary and the lifecycle log-marker format.
//!
//! Every managed service — the gateway, app-managed PostgreSQL, the
//! llama-server backends, the log receiver — in both process mode and
//! Windows-service (SCM) mode reports its lifecycle in the same words and
//! writes the same marker lines into its own log scope. This module is the
//! single home for both, and it lives in `manager-supervisor` because that
//! is the lowest crate every consumer (the app, the wizard, the service
//! shim) can depend on.
//!
//! - [`LifecycleState`]: the wire vocabulary every status payload uses
//!   (serialized snake_case). [`ProcessState`] stays the supervisor-internal
//!   process state and converts into it; "restarting" never comes from the
//!   supervisor, only from a transition record ([`TransitionKind`]).
//! - [`Origin`]: who asked for a lifecycle operation.
//! - [`marker_line`] / [`parse_line_ts`] / [`append_marker`]: the marker
//!   format (`[<rfc3339>] [lifecycle] <service>: <event>[: <detail>][ — by
//!   <origin>]`), its timestamp parser and a best-effort, rotating appender.
//! - [`prev_log_path`] / [`open_rotated_log`]: the one-previous-run log
//!   rotation shared by the wizard's process-mode spawns and the shim.
//! - [`process_start_time`]: OS creation time of a pid, so adopted processes
//!   report a real uptime.

use crate::ProcessState;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Size past which [`append_marker`] rotates a lifecycle log to its
/// `.prev` sibling (1 MiB, the same budget `wizard.log` uses).
pub const MARKER_LOG_ROTATE_BYTES: u64 = 1_048_576;

/// The tag every lifecycle marker line carries after its timestamp.
pub const MARKER_TAG: &str = "[lifecycle]";

/// Lifecycle state of one managed service, as every status surface reports
/// it. One vocabulary for every service and both run modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Up and serving.
    Running,
    /// Coming up: spawned (or SCM START_PENDING) but not ready yet.
    Starting,
    /// An operator restart is in flight (stop then start under one guard).
    Restarting,
    /// Going down: a stop is in flight (or SCM STOP_PENDING).
    Stopping,
    /// Down because it was stopped (or never started).
    Stopped,
    /// Down unexpectedly: crashed, failed to start, or could not be
    /// confirmed stopped.
    Failed,
    /// Process alive but not answering (or SCM PAUSED).
    Degraded,
    /// State could not be determined.
    Unknown,
    /// Service mode only: no SCM registration exists.
    NotInstalled,
}

impl LifecycleState {
    /// Every state, in display order.
    pub const ALL: [LifecycleState; 9] = [
        LifecycleState::Running,
        LifecycleState::Starting,
        LifecycleState::Restarting,
        LifecycleState::Stopping,
        LifecycleState::Stopped,
        LifecycleState::Failed,
        LifecycleState::Degraded,
        LifecycleState::Unknown,
        LifecycleState::NotInstalled,
    ];

    /// The wire string (identical to the serde form).
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleState::Running => "running",
            LifecycleState::Starting => "starting",
            LifecycleState::Restarting => "restarting",
            LifecycleState::Stopping => "stopping",
            LifecycleState::Stopped => "stopped",
            LifecycleState::Failed => "failed",
            LifecycleState::Degraded => "degraded",
            LifecycleState::Unknown => "unknown",
            LifecycleState::NotInstalled => "not_installed",
        }
    }

    /// Parse a wire string; also accepts the legacy SCM spellings
    /// (`start_pending`, `stop_pending`, `paused`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "running" | "up" => LifecycleState::Running,
            "starting" | "start_pending" | "continue_pending" => LifecycleState::Starting,
            "restarting" => LifecycleState::Restarting,
            "stopping" | "stop_pending" => LifecycleState::Stopping,
            "stopped" | "down" => LifecycleState::Stopped,
            "failed" => LifecycleState::Failed,
            "degraded" | "paused" | "pause_pending" => LifecycleState::Degraded,
            "unknown" => LifecycleState::Unknown,
            "not_installed" | "notinstalled" => LifecycleState::NotInstalled,
            _ => return None,
        })
    }

    /// Up or coming up: the service is (or is about to be) present and
    /// expected to serve. Stopping counts as on its way down, not live.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            LifecycleState::Running
                | LifecycleState::Starting
                | LifecycleState::Restarting
                | LifecycleState::Degraded
        )
    }

    /// A transitional state: starting, restarting or stopping. Status
    /// surfaces show these instead of "down" while an operation runs.
    pub fn is_transition(self) -> bool {
        matches!(
            self,
            LifecycleState::Starting | LifecycleState::Restarting | LifecycleState::Stopping
        )
    }
}

impl std::fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<ProcessState> for LifecycleState {
    fn from(s: ProcessState) -> Self {
        match s {
            ProcessState::Stopped => LifecycleState::Stopped,
            ProcessState::Starting => LifecycleState::Starting,
            ProcessState::Running => LifecycleState::Running,
            ProcessState::Stopping => LifecycleState::Stopping,
            ProcessState::Failed => LifecycleState::Failed,
        }
    }
}

/// The kind of an in-flight lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionKind {
    Starting,
    Stopping,
    Restarting,
}

impl TransitionKind {
    /// The wire string (identical to the serde form).
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionKind::Starting => "starting",
            TransitionKind::Stopping => "stopping",
            TransitionKind::Restarting => "restarting",
        }
    }

    /// The operation's verb, as markers phrase it ("restart requested").
    pub fn verb(self) -> &'static str {
        match self {
            TransitionKind::Starting => "start",
            TransitionKind::Stopping => "stop",
            TransitionKind::Restarting => "restart",
        }
    }
}

impl std::fmt::Display for TransitionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<TransitionKind> for LifecycleState {
    fn from(k: TransitionKind) -> Self {
        match k {
            TransitionKind::Starting => LifecycleState::Starting,
            TransitionKind::Stopping => LifecycleState::Stopping,
            TransitionKind::Restarting => LifecycleState::Restarting,
        }
    }
}

/// Who asked for a lifecycle operation (or who observed a change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Ui,
    Dashboard,
    Activation,
    Api,
    Cli,
    Wizard,
    Bench,
    Optimize,
    Monitor,
    Startup,
    Scm,
}

impl Origin {
    /// The wire string (identical to the serde form).
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::Dashboard => "dashboard",
            Origin::Activation => "activation",
            Origin::Api => "api",
            Origin::Cli => "cli",
            Origin::Wizard => "wizard",
            Origin::Bench => "bench",
            Origin::Optimize => "optimize",
            Origin::Monitor => "monitor",
            Origin::Startup => "startup",
            Origin::Scm => "scm",
        }
    }

    /// Parse an origin string, `None` when it names no known origin.
    /// Case-insensitive; accepts a few aliases (`activation-plan`,
    /// `rest`, `service`).
    pub fn try_parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "ui" => Origin::Ui,
            "dashboard" => Origin::Dashboard,
            "activation" | "activation-plan" | "activation_plan" => Origin::Activation,
            "api" | "rest" => Origin::Api,
            "cli" => Origin::Cli,
            "wizard" => Origin::Wizard,
            "bench" => Origin::Bench,
            "optimize" => Origin::Optimize,
            "monitor" => Origin::Monitor,
            "startup" => Origin::Startup,
            "scm" | "service" => Origin::Scm,
            _ => return None,
        })
    }

    /// Parse an origin string; unknown values fall back to [`Origin::Api`]
    /// (an unrecognized caller is an API client).
    pub fn parse(s: &str) -> Origin {
        Self::try_parse(s).unwrap_or(Origin::Api)
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Collapse line breaks so one marker is always exactly one log line.
fn one_line(s: &str) -> String {
    s.split(['\r', '\n'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format one lifecycle marker line (no trailing newline):
///
/// `[2026-11-12T10:00:00.123Z] [lifecycle] gateway: restart requested — by activation`
///
/// `detail`, when present, follows the event after `": "`. Line breaks in
/// any part are collapsed so the marker stays a single line.
pub fn marker_line(
    at: DateTime<Utc>,
    service: &str,
    event: &str,
    detail: Option<&str>,
    origin: Option<Origin>,
) -> String {
    let mut line = format!(
        "[{}] {MARKER_TAG} {}: {}",
        at.to_rfc3339_opts(SecondsFormat::Millis, true),
        one_line(service),
        one_line(event)
    );
    if let Some(d) = detail.map(one_line).filter(|d| !d.is_empty()) {
        line.push_str(": ");
        line.push_str(&d);
    }
    if let Some(o) = origin {
        line.push_str(" — by ");
        line.push_str(o.as_str());
    }
    line
}

/// The timestamp of a line that starts with `[<rfc3339>]` (marker lines,
/// `wizard.log` lines). `None` for any other line.
pub fn parse_line_ts(line: &str) -> Option<DateTime<Utc>> {
    let rest = line.strip_prefix('[')?;
    let end = rest.find(']')?;
    DateTime::parse_from_rfc3339(rest[..end].trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// True when `line` is a lifecycle marker (the `[lifecycle]` tag right
/// after a parseable timestamp).
pub fn is_marker_line(line: &str) -> bool {
    parse_line_ts(line).is_some()
        && line
            .find(']')
            .map(|i| line[i + 1..].trim_start().starts_with(MARKER_TAG))
            .unwrap_or(false)
}

/// Append one marker line to `path`. Best-effort: creates the parent
/// directory, rotates the file to [`prev_log_path`] once it is past
/// [`MARKER_LOG_ROTATE_BYTES`], and ignores every error (a lifecycle
/// operation never fails because its marker could not be written).
pub fn append_marker(path: &Path, line: &str) {
    append_marker_with_limit(path, line, MARKER_LOG_ROTATE_BYTES);
}

/// [`append_marker`] with an explicit rotation threshold (tests).
pub(crate) fn append_marker_with_limit(path: &Path, line: &str, limit: u64) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > limit {
            let prev = prev_log_path(path);
            let _ = std::fs::remove_file(&prev);
            let _ = std::fs::rename(path, &prev);
        }
    }
    let mut text = String::with_capacity(line.len() + 1);
    text.push_str(line.trim_end_matches(['\r', '\n']));
    text.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// The rotation sibling of a log file: `gateway.stdout.log` rotates
/// to `gateway.stdout.log.prev` (one previous run kept, matching the
/// log retention the Logs view documents).
pub fn prev_log_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".prev");
    PathBuf::from(s)
}

/// Rotate `path` to its [`prev_log_path`] sibling (replacing any older
/// `.prev`) without opening a new file. Creates parent directories. A
/// missing `path` is not an error. Callers that open the file
/// themselves (the service shim opens inheritable Win32 handles) use
/// this so both modes keep the same "one previous run" layout as
/// [`open_rotated_log`].
pub fn rotate_log(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let prev = prev_log_path(path);
        let _ = std::fs::remove_file(&prev);
        std::fs::rename(path, &prev)?;
    }
    Ok(())
}

/// Open `path` for appending, first rotating any existing file to its
/// `.prev` sibling. Creates parent directories as needed.
pub fn open_rotated_log(path: &Path) -> std::io::Result<std::fs::File> {
    rotate_log(path)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// OS creation time of `pid`. Windows: `GetProcessTimes`. Other
/// platforms: `None` (not resolved).
pub fn process_start_time(pid: u32) -> Option<SystemTime> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok =
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        filetime_to_system_time(
            ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// A FILETIME (100 ns ticks since 1601-01-01 UTC) as a `SystemTime`;
/// `None` for times before the Unix epoch (including a zero FILETIME).
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn filetime_to_system_time(ticks: u64) -> Option<SystemTime> {
    /// 100 ns ticks between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = ticks.checked_sub(EPOCH_DIFF_TICKS)?;
    let secs = since_unix / 10_000_000;
    let nanos = (since_unix % 10_000_000) * 100;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nanos as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mgr-sup-lifecycle-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 12, 10, 0, 0).unwrap() + chrono::Duration::milliseconds(123)
    }

    #[test]
    fn marker_line_exact_format() {
        assert_eq!(
            marker_line(
                at(),
                "gateway",
                "restart requested",
                None,
                Some(Origin::Activation)
            ),
            "[2026-11-12T10:00:00.123Z] [lifecycle] gateway: restart requested — by activation"
        );
        assert_eq!(
            marker_line(
                at(),
                "backend:planner",
                "start failed",
                Some("port 8081\nin use"),
                None
            ),
            "[2026-11-12T10:00:00.123Z] [lifecycle] backend:planner: start failed: port 8081 in use"
        );
    }

    #[test]
    fn marker_line_parse_line_ts_round_trip() {
        let line = marker_line(
            at(),
            "postgres",
            "stopped",
            Some("pid 42"),
            Some(Origin::Ui),
        );
        assert_eq!(parse_line_ts(&line), Some(at()));
        assert!(is_marker_line(&line));
        // wizard.log style lines parse too (any offset is normalized).
        assert_eq!(
            parse_line_ts("[2026-11-12T11:00:00.123+01:00] === step start-gateway ==="),
            Some(at())
        );
        assert!(!is_marker_line("[2026-11-12T10:00:00Z] plain line"));
        // Lines without a leading rfc3339 bracket are not stamped.
        assert_eq!(parse_line_ts("INFO: started server process"), None);
        assert_eq!(parse_line_ts("[not a time] x"), None);
        assert_eq!(parse_line_ts(" [2026-11-12T10:00:00Z] x"), None);
        assert_eq!(parse_line_ts("[2026-11-12T10:00:00Z"), None);
    }

    #[test]
    fn vocabulary_wire_strings_match_serde() {
        for s in LifecycleState::ALL {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(LifecycleState::parse(s.as_str()), Some(s));
            let back: LifecycleState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        assert_eq!(LifecycleState::NotInstalled.as_str(), "not_installed");
        assert_eq!(
            LifecycleState::parse("STOP_PENDING"),
            Some(LifecycleState::Stopping)
        );
        assert_eq!(LifecycleState::parse("bogus"), None);
        for o in [
            Origin::Ui,
            Origin::Dashboard,
            Origin::Activation,
            Origin::Api,
            Origin::Cli,
            Origin::Wizard,
            Origin::Bench,
            Origin::Optimize,
            Origin::Monitor,
            Origin::Startup,
            Origin::Scm,
        ] {
            assert_eq!(
                serde_json::to_string(&o).unwrap(),
                format!("\"{}\"", o.as_str())
            );
            assert_eq!(Origin::parse(o.as_str()), o);
        }
        assert_eq!(Origin::parse("Activation-Plan"), Origin::Activation);
        assert_eq!(Origin::parse("who-knows"), Origin::Api);
        assert_eq!(Origin::try_parse("who-knows"), None);
        assert_eq!(
            serde_json::to_string(&TransitionKind::Restarting).unwrap(),
            "\"restarting\""
        );
    }

    #[test]
    fn state_conversions_and_predicates() {
        assert_eq!(
            LifecycleState::from(TransitionKind::Restarting),
            LifecycleState::Restarting
        );
        assert_eq!(TransitionKind::Stopping.verb(), "stop");
        assert_eq!(
            LifecycleState::from(ProcessState::Failed),
            LifecycleState::Failed
        );
        assert_eq!(
            LifecycleState::from(ProcessState::Running),
            LifecycleState::Running
        );
        assert!(LifecycleState::Restarting.is_transition());
        assert!(LifecycleState::Stopping.is_transition());
        assert!(!LifecycleState::Failed.is_transition());
        assert!(LifecycleState::Degraded.is_live());
        assert!(!LifecycleState::Stopping.is_live());
        assert!(!LifecycleState::NotInstalled.is_live());
    }

    #[test]
    fn append_marker_appends_and_creates_parent() {
        let dir = temp_dir("append");
        let path = dir.join("logs").join("gateway.lifecycle.log");
        append_marker(&path, "first");
        append_marker(&path, "second\n");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "first\nsecond\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_marker_rotates_past_threshold() {
        let dir = temp_dir("rotate");
        let path = dir.join("svc.lifecycle.log");
        // At the limit: no rotation yet (rotation is strictly past it).
        append_marker_with_limit(&path, "123456789", 10); // 10 bytes
        append_marker_with_limit(&path, "a", 10); // 12 bytes now
        assert!(!prev_log_path(&path).exists());
        // Past the limit: the next append rotates first.
        append_marker_with_limit(&path, "fresh", 10);
        assert_eq!(
            std::fs::read_to_string(prev_log_path(&path)).unwrap(),
            "123456789\na\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        // The default threshold is the 1 MiB budget.
        let big = dir.join("big.lifecycle.log");
        std::fs::write(&big, vec![b'x'; MARKER_LOG_ROTATE_BYTES as usize + 1]).unwrap();
        append_marker(&big, "after");
        assert_eq!(std::fs::read_to_string(&big).unwrap(), "after\n");
        assert_eq!(
            std::fs::metadata(prev_log_path(&big)).unwrap().len(),
            MARKER_LOG_ROTATE_BYTES + 1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_rotated_log_keeps_one_previous_run() {
        let dir = temp_dir("rotated");
        let path = dir.join("nested").join("gateway.stdout.log");
        {
            let mut f = open_rotated_log(&path).unwrap();
            f.write_all(b"run1\n").unwrap();
        }
        {
            let mut f = open_rotated_log(&path).unwrap();
            f.write_all(b"run2\n").unwrap();
        }
        {
            let mut f = open_rotated_log(&path).unwrap();
            f.write_all(b"run3\n").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "run3\n");
        assert_eq!(
            std::fs::read_to_string(prev_log_path(&path)).unwrap(),
            "run2\n"
        );
        assert_eq!(
            prev_log_path(Path::new("a/b.log")),
            PathBuf::from("a/b.log.prev")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_log_moves_to_prev_without_creating_a_new_file() {
        let dir = temp_dir("rotate-only");
        let path = dir.join("sub").join("backend-x.stdout.log");
        // Missing file: not an error, parent created, nothing written.
        rotate_log(&path).unwrap();
        assert!(path.parent().unwrap().is_dir());
        assert!(!path.exists());
        std::fs::write(&path, "old\n").unwrap();
        rotate_log(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(prev_log_path(&path)).unwrap(), "old\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_system_time(0), None);
        assert_eq!(
            filetime_to_system_time(116_444_736_000_000_000),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            filetime_to_system_time(116_444_736_000_000_000 + 10_000_005),
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(1, 500))
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_start_time_of_self_is_in_the_past() {
        let t = process_start_time(std::process::id()).expect("own start time");
        let now = SystemTime::now();
        assert!(t <= now);
        assert!(now.duration_since(t).unwrap() < std::time::Duration::from_secs(3600));
        // A pid that cannot exist resolves to None.
        assert_eq!(process_start_time(u32::MAX - 1), None);
    }
}
