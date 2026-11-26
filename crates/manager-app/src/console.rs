//! The debug console (docs/debug-console.md): recipes, paste import,
//! controlled runs, structured results, the built-in command line and
//! the opt-in raw shell. The pure logic (recipe model, paste parser,
//! output parsers, NTSTATUS decoding, command grammar) lives in
//! `manager-diag`; this module owns state, processes, backends,
//! persistence, audit and the security gates.
//!
//! Every capability is a registry operation, so the desktop UI and the
//! admin API offer the same surface. Security (design §7):
//!
//! - **Who:** REST requests carry a `caller` argument the API layer
//!   fills from the authenticated principal (clients cannot supply
//!   it); a desktop call has none and is the local operator.
//! - **Where:** anything that starts processes or changes backend
//!   state (recipe runs, lifecycle commands, `set-flags`, the raw
//!   shell) is refused for non-loopback API requests unless
//!   `diagnostics.allow_remote_exec` is set.
//! - **What:** recipe steps run only allowlisted executables
//!   (`diagnostics.exec_allowlist`, default: the managed llama.cpp
//!   binaries and `nvidia-smi`), always as argv arrays — never through
//!   a shell. The raw shell is a separate mode: per session in the UI,
//!   and over the API only when `diagnostics.api_raw_shell` is set.
//! - **Audit:** every import, recipe change, run (with its resolved
//!   command lines) and console command is appended to
//!   `logs/admin-api-audit.jsonl` with the actor.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::registry::{role_allows, CapClass, Role};
use crate::{ApiError, ApiResult, AppState, EventSink};

mod commands;
mod plan;
mod runner;
mod shell;
mod store;
#[cfg(test)]
mod tests;

pub use plan::Plan;

/// Event carrying live console output (`{run_id, seq, step, stream,
/// line, ts_ms}`); also served by `console_run_output` polling.
pub const CONSOLE_OUTPUT_EVENT: &str = "console-output";
/// Event carrying a run's status changes (`{run_id, status, ...}`).
pub const CONSOLE_RUN_EVENT: &str = "console-run";

// ---------------------------------------------------------------------------
// Caller identity
// ---------------------------------------------------------------------------

/// Who is calling. Filled by the admin API from the authenticated
/// principal; `None` (the desktop shell) is the local operator.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Caller {
    /// `ui` or `api`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub auth_type: String,
    #[serde(default)]
    pub source_ip: String,
    #[serde(default)]
    pub loopback: bool,
    /// `viewer` | `operator` | `configurator` | `admin`.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub request_id: String,
}

impl Caller {
    pub(crate) fn from_arg(v: Option<Value>) -> Caller {
        match v.and_then(|v| serde_json::from_value::<Caller>(v).ok()) {
            Some(mut c) => {
                if c.kind.is_empty() {
                    c.kind = "api".into();
                }
                c
            }
            None => Caller {
                kind: "ui".into(),
                actor: "desktop".into(),
                auth_type: "desktop".into(),
                source_ip: "local".into(),
                loopback: true,
                role: "admin".into(),
                request_id: String::new(),
            },
        }
    }

    pub(crate) fn is_api(&self) -> bool {
        self.kind != "ui"
    }

    fn role(&self) -> Role {
        match self.role.as_str() {
            "admin" => Role::Admin,
            "configurator" => Role::Configurator,
            "operator" => Role::Operator,
            _ => Role::Viewer,
        }
    }

    /// The role check for one console capability class (console_exec
    /// is a read-class route; each command checks its own class).
    pub(crate) fn require_class(&self, class: CapClass, what: &str) -> Result<(), ApiError> {
        if self.is_api() && !role_allows(self.role(), class) {
            return Err(ApiError::Forbidden(format!(
                "the authenticated role may not {what}"
            )));
        }
        Ok(())
    }

    /// Short label for output and audit (`ui`, `api:alice`).
    pub(crate) fn label(&self) -> String {
        if self.is_api() {
            format!("api:{}", self.actor)
        } else {
            "ui".into()
        }
    }
}

/// Refuse a process-running action for a non-loopback API caller
/// unless the operator allowed remote execution.
pub(crate) async fn require_exec(
    state: &AppState,
    caller: &Caller,
    what: &str,
) -> Result<(), ApiError> {
    if !caller.is_api() || caller.loopback {
        return Ok(());
    }
    if state.config.read().await.diagnostics.allow_remote_exec {
        return Ok(());
    }
    Err(ApiError::Forbidden(format!(
        "{what} runs processes on this machine and is refused for non-loopback admin API \
         requests (from {}); enable Settings → Debug console → \"Allow remote execution\" \
         (diagnostics.allow_remote_exec) to permit it",
        caller.source_ip
    )))
}

/// Append a console record to the admin audit log.
pub(crate) fn audit(
    state: &AppState,
    caller: &Caller,
    action: &str,
    target: Option<&str>,
    result: &str,
    detail: Value,
) {
    crate::audit::append(
        &state.data_dir,
        &json!({
            "ts_ms": chrono::Utc::now().timestamp_millis(),
            "actor": if caller.is_api() { caller.actor.clone() } else { "desktop-ui".to_string() },
            "auth_type": if caller.is_api() { caller.auth_type.clone() } else { "desktop".to_string() },
            "source_ip": caller.source_ip,
            "action": action,
            "target": target,
            "result": result,
            "request_id": caller.request_id,
            "origin": if caller.is_api() { "api" } else { "ui" },
            "detail": detail,
        }),
    );
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Console state held by `AppState`.
#[derive(Default)]
pub struct ConsoleState {
    /// The active run (one at a time: runs hold GPUs and stop backends).
    pub(crate) active: tokio::sync::Mutex<Option<runner::ActiveRun>>,
    /// Live output buffers of recent runs (in-memory tail; the full
    /// output is in the run directory's `output.jsonl`).
    pub(crate) live: std::sync::Mutex<HashMap<String, Arc<runner::RunLive>>>,
    /// Raw shell sessions by session id.
    pub(crate) shells: tokio::sync::Mutex<HashMap<String, shell::ShellSession>>,
}

/// The active console run's id, if one is running (Bench refuses to
/// start alongside it).
pub(crate) async fn active_run_id(state: &AppState) -> Option<String> {
    runner::active_id(state).await
}

fn opt_map(v: Option<BTreeMap<String, String>>) -> BTreeMap<String, String> {
    v.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Operations (registry handlers)
// ---------------------------------------------------------------------------

/// Built-in and saved recipes.
pub async fn op_console_recipes_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    Ok(json!({ "recipes": store::list_recipes(&state.data_dir) }))
}

/// One recipe document.
pub async fn op_console_recipe_get(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    name: String,
) -> ApiResult {
    Ok(serde_json::to_value(store::load_recipe(
        &state.data_dir,
        &name,
    )?)?)
}

/// Save a new recipe (refuses an existing name unless `overwrite`).
pub async fn op_console_recipe_create(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    recipe: Value,
    overwrite: Option<bool>,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    let r: manager_diag::Recipe = serde_json::from_value(recipe)
        .map_err(|e| ApiError::InvalidInput(format!("recipe: {e}")))?;
    let saved = store::save_recipe(&state.data_dir, &r, overwrite.unwrap_or(false));
    audit(
        state,
        &caller,
        "console.recipe.save",
        Some(&r.name),
        if saved.is_ok() { "ok" } else { "refused" },
        json!({ "steps": r.steps.len() }),
    );
    saved?;
    Ok(json!({ "saved": true, "name": r.name }))
}

/// Replace a saved recipe.
pub async fn op_console_recipe_update(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    name: String,
    recipe: Value,
    caller: Option<Value>,
) -> ApiResult {
    let doc_name = recipe.get("name").and_then(Value::as_str).unwrap_or("");
    if doc_name != name {
        return Err(ApiError::InvalidInput(format!(
            "the recipe document is named {doc_name:?}, not {name:?} (rename by creating a new recipe and deleting the old one)"
        )));
    }
    op_console_recipe_create(state, sink, recipe, Some(true), caller).await
}

/// Delete a saved recipe (built-ins cannot be deleted).
pub async fn op_console_recipe_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    name: String,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    let r = store::delete_recipe(&state.data_dir, &name);
    audit(
        state,
        &caller,
        "console.recipe.delete",
        Some(&name),
        if r.is_ok() { "ok" } else { "refused" },
        json!({}),
    );
    r?;
    Ok(json!({ "deleted": true, "name": name }))
}

/// Parse a pasted command block into a draft recipe for review. Never
/// runs anything; saves only when `save` is set.
pub async fn op_console_import(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    text: String,
    name: Option<String>,
    fold: Option<bool>,
    save: Option<bool>,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    if text.len() > 256 * 1024 {
        return Err(ApiError::InvalidInput(
            "paste is larger than 256 KiB".into(),
        ));
    }
    let ctx = plan::import_context(state).await;
    let result = manager_diag::paste::import(&text, &ctx, name.as_deref(), fold.unwrap_or(true));
    let mut out = serde_json::to_value(&result)?;
    // Resolve the draft against this machine so the review shows the
    // real paths, GPUs and conflicts before anything is saved or run.
    let preview = plan::build_plan(
        state,
        &result.recipe,
        None,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .await
    .map(|p| p.to_review_json())
    .unwrap_or_else(|e| json!({ "error": e.to_string() }));
    out["plan"] = preview;
    let mut saved = false;
    if save.unwrap_or(false) {
        store::save_recipe(&state.data_dir, &result.recipe, false)?;
        saved = true;
    }
    out["saved"] = json!(saved);
    audit(
        state,
        &caller,
        "console.import",
        Some(&result.recipe.name),
        "ok",
        json!({
            "dialect": result.dialect,
            "steps": result.recipe.steps.len(),
            "folds": result.folds.len(),
            "saved": saved,
            "paste_bytes": text.len(),
        }),
    );
    Ok(out)
}

/// Resolve a recipe against this machine without running it.
pub async fn op_console_plan(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    recipe: Option<String>,
    recipe_doc: Option<Value>,
    variant: Option<usize>,
    params: Option<BTreeMap<String, String>>,
    set: Option<BTreeMap<String, String>>,
) -> ApiResult {
    let r = store::recipe_arg(&state.data_dir, recipe, recipe_doc)?;
    let p = plan::build_plan(state, &r, variant, &opt_map(params), &opt_map(set)).await?;
    Ok(p.to_review_json())
}

/// Start a run. Answers at once with the run id (output streams via
/// `console_run_output` / `console-output` events) — or, when running
/// backends must be stopped and the caller has not confirmed them,
/// with `started: false` and the plan naming them.
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_console_run_start(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    recipe: Option<String>,
    recipe_doc: Option<Value>,
    variant: Option<usize>,
    params: Option<BTreeMap<String, String>>,
    set: Option<BTreeMap<String, String>>,
    confirm_stop: Option<Vec<String>>,
    confirm_all: Option<bool>,
    restore: Option<bool>,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    let r = store::recipe_arg(&state.data_dir, recipe, recipe_doc)?;
    runner::start(
        state,
        sink,
        &caller,
        runner::StartRequest {
            recipe: r,
            variant,
            params: opt_map(params),
            set: opt_map(set),
            confirm_stop: confirm_stop.unwrap_or_default(),
            confirm_all: confirm_all.unwrap_or(false),
            restore,
        },
    )
    .await
}

/// Cancel a run (the active one when `run_id` names it). Stopped
/// backends are still restored.
pub async fn op_console_run_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    run_id: String,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    let r = runner::cancel(state, Some(&run_id)).await;
    audit(
        state,
        &caller,
        "console.run.cancel",
        Some(&run_id),
        if r.is_ok() { "ok" } else { "refused" },
        json!({}),
    );
    r
}

/// Output lines after `since` (a sequence number). With `wait_ms`,
/// waits (up to 25 s) for new output or the end of the run — the
/// long-poll form for agents.
pub async fn op_console_run_output(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    run_id: String,
    since: Option<u64>,
    wait_ms: Option<u64>,
    limit: Option<u64>,
) -> ApiResult {
    runner::output(state, &run_id, since.unwrap_or(0), wait_ms, limit).await
}

/// A run's results: compact by default (parsed results, results
/// table, stderr excerpts), everything with `full`.
pub async fn op_console_run_result(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    run_id: String,
    full: Option<bool>,
) -> ApiResult {
    store::run_result(&state.data_dir, &run_id, full.unwrap_or(false))
}

/// Run history, newest first.
pub async fn op_console_runs_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    limit: Option<u64>,
) -> ApiResult {
    Ok(json!({
        "runs": store::list_runs(&state.data_dir, limit.unwrap_or(50) as usize),
        "active": runner::active_id(state).await,
    }))
}

/// The full JSON export of a run (also written as `export.json` in
/// the run directory).
pub async fn op_console_run_export(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    run_id: String,
) -> ApiResult {
    store::export_run(&state.data_dir, &run_id)
}

/// Run one built-in console command line.
pub async fn op_console_exec(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    line: String,
    session_id: Option<String>,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    commands::exec(state, sink, &caller, &line, session_id.as_deref()).await
}

/// A backend's persisted output (current run, or the previous one),
/// optionally filtered, newest lines last.
pub async fn op_console_backend_logs(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    prev: Option<bool>,
    grep: Option<String>,
    tail: Option<u64>,
    stream: Option<String>,
) -> ApiResult {
    commands::backend_logs(
        state,
        &id,
        prev.unwrap_or(false),
        grep.as_deref(),
        tail.unwrap_or(200) as usize,
        stream.as_deref().unwrap_or("stderr"),
    )
    .await
}

/// The console's security settings.
pub async fn op_console_settings_get(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await;
    Ok(json!({
        "allow_remote_exec": cfg.diagnostics.allow_remote_exec,
        "api_raw_shell": cfg.diagnostics.api_raw_shell,
        "exec_allowlist": cfg.diagnostics.exec_allowlist,
        "default_exec_allowlist": manager_config::default_exec_allowlist(),
    }))
}

/// Change the console's security settings. Desktop-only (no REST
/// route): an API caller must never be able to widen its own gates.
pub async fn op_console_settings_set(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    allow_remote_exec: Option<bool>,
    api_raw_shell: Option<bool>,
    exec_allowlist: Option<Vec<String>>,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    if caller.is_api() {
        return Err(ApiError::Forbidden(
            "debug console security settings can only be changed in the desktop app".into(),
        ));
    }
    if let Some(list) = &exec_allowlist {
        for e in list {
            if e.trim().is_empty() {
                return Err(ApiError::InvalidInput("empty allowlist entry".into()));
            }
            let lower = e.to_ascii_lowercase();
            if [
                ".bat",
                ".cmd",
                ".ps1",
                ".vbs",
                ".js",
                "cmd",
                "cmd.exe",
                "powershell",
                "powershell.exe",
                "pwsh",
                "pwsh.exe",
            ]
            .iter()
            .any(|bad| {
                lower == *bad
                    || lower.ends_with(&format!("/{bad}"))
                    || lower.ends_with(&format!("\\{bad}"))
                    || (bad.starts_with('.') && lower.ends_with(bad))
            }) {
                return Err(ApiError::InvalidInput(format!(
                    "{e:?} is a shell or script interpreter; recipe steps never run through a shell (use the raw shell mode instead)"
                )));
            }
        }
    }
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    if let Some(v) = allow_remote_exec {
        new_cfg.diagnostics.allow_remote_exec = v;
    }
    if let Some(v) = api_raw_shell {
        new_cfg.diagnostics.api_raw_shell = v;
    }
    if let Some(list) = exec_allowlist {
        new_cfg.diagnostics.exec_allowlist =
            list.into_iter().map(|s| s.trim().to_string()).collect();
    }
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    let d = new_cfg.diagnostics.clone();
    *cfg = new_cfg;
    drop(cfg);
    audit(
        state,
        &caller,
        "console.settings",
        None,
        "ok",
        json!({ "allow_remote_exec": d.allow_remote_exec, "api_raw_shell": d.api_raw_shell, "exec_allowlist": d.exec_allowlist }),
    );
    Ok(json!({
        "allow_remote_exec": d.allow_remote_exec,
        "api_raw_shell": d.api_raw_shell,
        "exec_allowlist": d.exec_allowlist,
    }))
}

/// Open a raw shell session (pwsh). Desktop: per session, on the
/// operator's toggle. API: only with `diagnostics.api_raw_shell`.
pub async fn op_console_shell_open(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    session_id: String,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    shell::open(state, &caller, &session_id).await
}

/// Send one line to a raw shell session.
pub async fn op_console_shell_send(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    session_id: String,
    line: String,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    shell::send(state, &caller, &session_id, &line).await
}

/// Read a raw shell session's output after `since`.
pub async fn op_console_shell_read(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    session_id: String,
    since: Option<u64>,
    wait_ms: Option<u64>,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    shell::read(state, &caller, &session_id, since.unwrap_or(0), wait_ms).await
}

/// Close a raw shell session (kills the shell).
pub async fn op_console_shell_close(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    session_id: String,
    caller: Option<Value>,
) -> ApiResult {
    let caller = Caller::from_arg(caller);
    shell::close(state, &caller, &session_id).await
}
