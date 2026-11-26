//! The raw shell: a separate console mode, off by default.
//!
//! A session is one PowerShell process (`pwsh`, falling back to
//! Windows PowerShell; `sh` elsewhere) reading commands from a pipe —
//! line-oriented, no terminal emulation, so its output is plain text
//! the console pane and agents can read. The desktop enables it per
//! console session on the operator's toggle; the admin API may open
//! one only when `diagnostics.api_raw_shell` is set (and, like every
//! process-running action, from loopback unless remote execution is
//! allowed). Every line sent is audited. Sessions are owned by their
//! opener: an API principal cannot type into the desktop's shell.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{audit, Caller};
use crate::registry::CapClass;
use crate::{ApiError, ApiResult, AppState};

const MAX_SESSIONS: usize = 4;
const KEEP_LINES: usize = 10_000;

pub(crate) struct ShellSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    buf: Arc<Mutex<VecDeque<Value>>>,
    next: Arc<Mutex<u64>>,
    notify: Arc<tokio::sync::Notify>,
    owner: String,
    program: String,
    opened: Instant,
}

fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn gate(state: &AppState, caller: &Caller) -> Result<(), ApiError> {
    caller.require_class(CapClass::D, "use the raw shell")?;
    if caller.is_api() && !state.config.read().await.diagnostics.api_raw_shell {
        return Err(ApiError::Forbidden(
            "the raw shell is disabled for the admin API (diagnostics.api_raw_shell); use built-in commands and recipes, or enable it in the desktop app".into(),
        ));
    }
    super::require_exec(state, caller, "the raw shell").await
}

fn shell_program(env: &std::collections::BTreeMap<String, String>) -> (String, Vec<String>) {
    if cfg!(windows) {
        let prog = super::plan::which("pwsh", env)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "powershell.exe".to_string());
        (
            prog,
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    } else {
        ("sh".to_string(), Vec::new())
    }
}

fn push(
    buf: &Mutex<VecDeque<Value>>,
    next: &Mutex<u64>,
    notify: &tokio::sync::Notify,
    stream: &str,
    line: String,
) {
    let mut n = next.lock().unwrap_or_else(|e| e.into_inner());
    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
    b.push_back(json!({ "seq": *n, "stream": stream, "line": line, "ts_ms": chrono::Utc::now().timestamp_millis() }));
    *n += 1;
    if b.len() > KEEP_LINES {
        b.pop_front();
    }
    notify.notify_waiters();
}

pub(crate) async fn open(state: &AppState, caller: &Caller, session_id: &str) -> ApiResult {
    gate(state, caller).await?;
    if !valid_session(session_id) {
        return Err(ApiError::InvalidInput(
            "session id must be 1-64 chars of [A-Za-z0-9_-]".into(),
        ));
    }
    let mut shells = state.console.shells.lock().await;
    if let Some(s) = shells.get(session_id) {
        if s.owner != caller.label() {
            return Err(ApiError::Forbidden(
                "that shell session belongs to another caller".into(),
            ));
        }
        return Ok(
            json!({ "session_id": session_id, "open": true, "program": s.program, "reused": true }),
        );
    }
    if shells.len() >= MAX_SESSIONS {
        return Err(ApiError::Busy(format!(
            "{MAX_SESSIONS} raw shell sessions are open; close one first"
        )));
    }
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let (program, args) = shell_program(&env);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .current_dir(&state.data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd
        .spawn()
        .map_err(|e| ApiError::Io(format!("could not start {program}: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Io("shell stdin unavailable".into()))?;
    let buf = Arc::new(Mutex::new(VecDeque::new()));
    let next = Arc::new(Mutex::new(1u64));
    let notify = Arc::new(tokio::sync::Notify::new());
    for (stream, reader) in [
        (
            "stdout",
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ),
        (
            "stderr",
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ),
    ] {
        let Some(reader) = reader else { continue };
        let (buf, next, notify) = (Arc::clone(&buf), Arc::clone(&next), Arc::clone(&notify));
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push(&buf, &next, &notify, stream, line);
            }
            push(
                &buf,
                &next,
                &notify,
                "console",
                format!("[shell {stream} closed]"),
            );
        });
    }
    push(&buf, &next, &notify, "console", format!("[raw shell: {program}; every line is audited; commands run with your account's rights]"));
    shells.insert(
        session_id.to_string(),
        ShellSession {
            child,
            stdin,
            buf,
            next,
            notify,
            owner: caller.label(),
            program: program.clone(),
            opened: Instant::now(),
        },
    );
    drop(shells);
    audit(
        state,
        caller,
        "console.shell.open",
        Some(session_id),
        "ok",
        json!({ "program": program }),
    );
    Ok(json!({ "session_id": session_id, "open": true, "program": program, "reused": false }))
}

pub(crate) async fn send(
    state: &AppState,
    caller: &Caller,
    session_id: &str,
    line: &str,
) -> ApiResult {
    gate(state, caller).await?;
    if line.len() > 16 * 1024 || line.contains('\n') || line.contains('\r') {
        return Err(ApiError::InvalidInput(
            "send one line (at most 16 KiB) at a time".into(),
        ));
    }
    let mut shells = state.console.shells.lock().await;
    let s = shells
        .get_mut(session_id)
        .ok_or_else(|| ApiError::NotFound(format!("shell session {session_id}")))?;
    if s.owner != caller.label() {
        return Err(ApiError::Forbidden(
            "that shell session belongs to another caller".into(),
        ));
    }
    push(&s.buf, &s.next, &s.notify, "input", line.to_string());
    let r = async {
        s.stdin.write_all(line.as_bytes()).await?;
        s.stdin.write_all(b"\n").await?;
        s.stdin.flush().await
    }
    .await;
    drop(shells);
    audit(
        state,
        caller,
        "console.shell.send",
        Some(session_id),
        if r.is_ok() { "ok" } else { "error" },
        json!({ "line": line }),
    );
    r.map_err(|e| ApiError::Io(format!("shell input: {e}")))?;
    Ok(json!({ "sent": true }))
}

pub(crate) async fn read(
    state: &AppState,
    caller: &Caller,
    session_id: &str,
    since: u64,
    wait_ms: Option<u64>,
) -> ApiResult {
    caller.require_class(CapClass::D, "use the raw shell")?;
    let (buf, notify, alive, program, age) = {
        let mut shells = state.console.shells.lock().await;
        let s = shells
            .get_mut(session_id)
            .ok_or_else(|| ApiError::NotFound(format!("shell session {session_id}")))?;
        if s.owner != caller.label() {
            return Err(ApiError::Forbidden(
                "that shell session belongs to another caller".into(),
            ));
        }
        let alive = matches!(s.child.try_wait(), Ok(None));
        (
            Arc::clone(&s.buf),
            Arc::clone(&s.notify),
            alive,
            s.program.clone(),
            s.opened.elapsed().as_secs(),
        )
    };
    let deadline = Instant::now() + Duration::from_millis(wait_ms.unwrap_or(0).min(25_000));
    loop {
        let notified = notify.notified();
        let lines: Vec<Value> = buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|l| l["seq"].as_u64().unwrap_or(0) > since)
            .cloned()
            .collect();
        if !lines.is_empty() || !alive || Instant::now() >= deadline {
            let next = lines
                .last()
                .and_then(|l| l["seq"].as_u64())
                .unwrap_or(since);
            return Ok(
                json!({ "session_id": session_id, "alive": alive, "program": program, "age_secs": age, "lines": lines, "next": next }),
            );
        }
        let _ = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), notified)
            .await;
    }
}

pub(crate) async fn close(state: &AppState, caller: &Caller, session_id: &str) -> ApiResult {
    caller.require_class(CapClass::D, "use the raw shell")?;
    let mut shells = state.console.shells.lock().await;
    let Some(s) = shells.get(session_id) else {
        return Ok(json!({ "closed": false, "reason": "no such session" }));
    };
    if s.owner != caller.label() {
        return Err(ApiError::Forbidden(
            "that shell session belongs to another caller".into(),
        ));
    }
    let mut s = shells.remove(session_id).expect("present");
    drop(shells);
    let _ = s.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.child.wait()).await;
    audit(
        state,
        caller,
        "console.shell.close",
        Some(session_id),
        "ok",
        json!({}),
    );
    Ok(json!({ "closed": true }))
}
