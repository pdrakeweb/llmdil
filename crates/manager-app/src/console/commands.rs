//! Built-in console commands: `console_exec` parses one line with
//! `manager_diag::command` and runs it. Each command carries its own
//! capability class (the route itself is read-class), and the ones
//! that start processes or change backends pass the loopback gate.
//! Every line is audited. A command's result is `{ok, command, text,
//! data}`: `text` for a terminal, `data` for agents.

use std::path::PathBuf;
use std::sync::Arc;

use manager_diag::command::{self, Command, LifecycleAction};
use serde_json::{json, Value};

use super::{audit, plan, runner, store, Caller};
use crate::backend_lifecycle::{self, BackendOpOpts};
use crate::lifecycle::Origin;
use crate::registry::CapClass;
use crate::{ApiError, ApiResult, AppState, EventSink};

fn class_of(c: &Command) -> CapClass {
    match c {
        Command::Run { .. } | Command::Bench { .. } | Command::Cancel { .. } => CapClass::D,
        Command::Lifecycle { .. } => CapClass::L,
        Command::SetFlags { .. } => CapClass::C,
        Command::Flags { .. } | Command::Recipe { .. } => CapClass::SConfig,
        _ => CapClass::R,
    }
}

pub(crate) async fn exec(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    caller: &Caller,
    line: &str,
    session_id: Option<&str>,
) -> ApiResult {
    if line.len() > 16 * 1024 {
        return Err(ApiError::InvalidInput(
            "command line longer than 16 KiB".into(),
        ));
    }
    let cmd = match command::parse(line) {
        Ok(c) => c,
        Err(e) => {
            audit(
                state,
                caller,
                "console.exec",
                None,
                "parse error",
                json!({ "line": line, "session_id": session_id }),
            );
            return Ok(
                json!({ "ok": false, "line": line, "error": e, "text": format!("error: {e}") }),
            );
        }
    };
    let word = cmd.word();
    if let Err(e) = caller.require_class(class_of(&cmd), &format!("run `{word}`")) {
        audit(
            state,
            caller,
            "console.exec",
            Some(word),
            "forbidden",
            json!({ "line": line }),
        );
        return Err(e);
    }
    if cmd.runs_processes() {
        if let Err(e) = super::require_exec(state, caller, &format!("`{word}`")).await {
            audit(
                state,
                caller,
                "console.exec",
                Some(word),
                "refused: non-loopback",
                json!({ "line": line }),
            );
            return Err(e);
        }
    }
    let r = run(state, sink, caller, &cmd).await;
    audit(
        state,
        caller,
        "console.exec",
        Some(word),
        match &r {
            Ok(_) => "ok",
            Err(_) => "error",
        },
        json!({ "line": line, "session_id": session_id, "command": cmd }),
    );
    match r {
        Ok((text, data)) => {
            Ok(json!({ "ok": true, "line": line, "command": word, "text": text, "data": data }))
        }
        Err(ApiError::Forbidden(m)) => Err(ApiError::Forbidden(m)),
        Err(e) => Ok(
            json!({ "ok": false, "line": line, "command": word, "error": e.to_string(), "text": format!("error: {e}") }),
        ),
    }
}

fn render_plan(p: &Value) -> String {
    let mut t = String::new();
    t.push_str(&format!("{}\n", p["summary"].as_str().unwrap_or("")));
    for s in p["steps"].as_array().into_iter().flatten() {
        t.push_str(&format!(
            "  [{}] {}\n      {}\n",
            s["index"].as_u64().unwrap_or(0) + 1,
            s["label"].as_str().unwrap_or(""),
            s["command_line"].as_str().unwrap_or("")
        ));
        if let Some(env) = s["env"].as_object().filter(|e| !e.is_empty()) {
            t.push_str(&format!(
                "      env: {}\n",
                env.iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }
    for s in p["stop_backends"].as_array().into_iter().flatten() {
        t.push_str(&format!(
            "  stops {} ({}){}\n",
            s["id"].as_str().unwrap_or(""),
            s["reason"].as_str().unwrap_or(""),
            if p["restore"] == true {
                "; restored afterwards"
            } else {
                ""
            }
        ));
    }
    for w in p["warnings"].as_array().into_iter().flatten() {
        t.push_str(&format!("  warning: {}\n", w.as_str().unwrap_or("")));
    }
    for e in p["errors"].as_array().into_iter().flatten() {
        t.push_str(&format!("  ERROR: {}\n", e.as_str().unwrap_or("")));
    }
    t
}

fn render_start(v: &Value) -> String {
    if v["started"] == true {
        format!(
            "run {} started — follow with the output pane, `show {}` when it ends, `cancel` to stop it.\n{}",
            v["run_id"].as_str().unwrap_or(""),
            v["run_id"].as_str().unwrap_or(""),
            render_plan(&v["plan"])
        )
    } else {
        format!(
            "not started: {}\n{}",
            v["reason"].as_str().unwrap_or(""),
            render_plan(&v["plan"])
        )
    }
}

async fn run(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    caller: &Caller,
    cmd: &Command,
) -> Result<(String, Value), ApiError> {
    match cmd {
        Command::Help { .. } => Ok((command::HELP.to_string(), json!({ "help": command::HELP }))),
        Command::Recipes => {
            let list = store::list_recipes(&state.data_dir);
            let mut t = String::from("recipes:\n");
            for r in &list {
                t.push_str(&format!(
                    "  {:<24} {:>2} step(s) {:>2} variant(s)  {}{}\n",
                    r["name"].as_str().unwrap_or(""),
                    r["steps"].as_u64().unwrap_or(0),
                    r["variants"].as_u64().unwrap_or(1),
                    if r["builtin"] == true { "[built-in] " } else { "" },
                    r["title"].as_str().unwrap_or(""),
                ));
            }
            Ok((t, json!({ "recipes": list })))
        }
        Command::Recipe { name } => {
            let r = store::load_recipe(&state.data_dir, name)?;
            let v = serde_json::to_value(&r)?;
            Ok((serde_json::to_string_pretty(&v)?, v))
        }
        Command::Plan { recipe, variant, params, set } => {
            let r = store::load_recipe(&state.data_dir, recipe)?;
            let p = plan::build_plan(state, &r, *variant, params, set).await?.to_review_json();
            Ok((render_plan(&p), p))
        }
        Command::Run { recipe, variant, params, set, confirm_stop, yes, no_restore } => {
            let r = store::load_recipe(&state.data_dir, recipe)?;
            let v = runner::start(
                state,
                sink,
                caller,
                runner::StartRequest {
                    recipe: r,
                    variant: *variant,
                    params: params.clone(),
                    set: set.clone(),
                    confirm_stop: confirm_stop.clone(),
                    confirm_all: *yes,
                    restore: no_restore.then_some(false),
                },
            )
            .await?;
            Ok((render_start(&v), v))
        }
        Command::Bench { backend, depth, set, confirm_stop, yes } => {
            let r = store::load_recipe(&state.data_dir, "llama-bench-depth")?;
            let params = [
                ("backend".to_string(), backend.clone()),
                ("depth".to_string(), depth.clone()),
            ]
            .into();
            let v = runner::start(
                state,
                sink,
                caller,
                runner::StartRequest {
                    recipe: r,
                    variant: None,
                    params,
                    set: set.clone(),
                    confirm_stop: confirm_stop.clone(),
                    confirm_all: *yes,
                    restore: None,
                },
            )
            .await?;
            Ok((render_start(&v), v))
        }
        Command::Cancel { run_id } => {
            let v = runner::cancel(state, run_id.as_deref()).await?;
            Ok((
                format!("cancelling {} (stopped backends are still restored)", v["run_id"].as_str().unwrap_or("")),
                v,
            ))
        }
        Command::Import => Ok((
            "Paste a command block into the Import panel (desktop), or POST /api/v1/console/import {\"text\": \"…\"} — it becomes a draft recipe to review; nothing runs until you run it.".into(),
            json!({ "hint": "console_import" }),
        )),
        Command::Lifecycle { action, backend } => {
            if !state.config.read().await.backends.iter().any(|b| &b.id == backend) {
                return Err(ApiError::NotFound(format!("backend {backend}")));
            }
            let opts = BackendOpOpts {
                graceful: true,
                ..BackendOpOpts::new(Origin::Console)
            };
            let v = match action {
                LifecycleAction::Start => backend_lifecycle::start_backend(state, sink, backend, opts).await?,
                LifecycleAction::Stop => backend_lifecycle::stop_backend(state, sink, backend, opts).await?,
                LifecycleAction::Restart => backend_lifecycle::restart_backend(state, sink, backend, opts).await?,
            };
            Ok((format!("{} {backend}: requested", cmd.word()), v))
        }
        Command::Logs { backend, prev, grep, tail } => {
            let v = backend_logs(state, backend, *prev, grep.as_deref(), *tail, "stderr").await?;
            let text = v["text"].as_str().unwrap_or("").to_string();
            Ok((text, v))
        }
        Command::Gpu => {
            let store = state.telemetry().await;
            let (inv, snap) = {
                let s = crate::lock_telemetry(&store);
                (s.inventory(), s.snapshot())
            };
            let mut t = String::new();
            let mut rows = Vec::new();
            for g in &snap {
                let info = inv.iter().find(|i| i.index == g.index);
                t.push_str(&format!(
                    "GPU {} {}: {} / {} MiB used, {:.0}% util, {:.0} °C{}{}\n",
                    g.index,
                    g.name,
                    g.vram_used_mib,
                    g.vram_total_mib,
                    g.utilization_pct,
                    g.temp_c,
                    g.power_w.map(|p| format!(", {p:.0} W")).unwrap_or_default(),
                    if g.stale { " [stale]" } else { "" }
                ));
                rows.push(json!({
                    "index": g.index, "name": g.name, "uuid": info.map(|i| i.uuid.clone()),
                    "vram_used_mib": g.vram_used_mib, "vram_total_mib": g.vram_total_mib,
                    "util_pct": g.utilization_pct, "temp_c": g.temp_c, "power_w": g.power_w, "stale": g.stale,
                }));
            }
            if snap.is_empty() {
                t.push_str("no GPU telemetry yet\n");
            }
            Ok((t, json!({ "gpus": rows })))
        }
        Command::Flags { backend } => {
            let cfg = state.config.read().await;
            let b = cfg
                .backends
                .iter()
                .find(|b| &b.id == backend)
                .ok_or_else(|| ApiError::NotFound(format!("backend {backend}")))?;
            let v = json!({
                "id": b.id,
                "port": b.port,
                "model_file": b.model_file,
                "n_ctx": b.flags.n_ctx,
                "n_batch": b.flags.n_batch,
                "tensor_split": b.flags.tensor_split,
                "split_mode": b.flags.split_mode,
                "raw_flags": b.raw_flags,
                "flags_advanced": b.flags_advanced,
            });
            let t = format!(
                "{}: port {}, n_ctx {}, n_batch {}, tensor_split {:?}, split_mode {:?}\n  raw flags: {}\n  mode: {}\n",
                b.id,
                b.port,
                b.flags.n_ctx,
                b.flags.n_batch,
                b.flags.tensor_split,
                b.flags.split_mode,
                b.raw_flags.as_deref().unwrap_or("(none)"),
                if b.flags_advanced { "advanced (raw flags edited as text)" } else { "structured" }
            );
            Ok((t, v))
        }
        Command::SetFlags { backend, flags } => {
            let advanced = state
                .config
                .read()
                .await
                .backends
                .iter()
                .find(|b| &b.id == backend)
                .map(|b| b.flags_advanced)
                .ok_or_else(|| ApiError::NotFound(format!("backend {backend}")))?;
            let mut notes = Vec::new();
            if !advanced {
                // The raw-flags editor requires advanced mode; switching
                // it is its own journaled configuration change.
                crate::op_backend_set_flags_mode(state, sink, backend.clone(), true).await?;
                notes.push(format!("switched {backend} to advanced flags mode"));
            }
            let v = crate::op_backend_set_raw_flags(
                state,
                sink,
                backend.clone(),
                Some(flags.clone()),
            )
            .await?;
            let t = format!(
                "{backend}: raw flags {} (journaled in the activation plan; applies on the next start — restart {backend} to apply){}{}",
                if v["changed"] == true { "updated" } else { "unchanged" },
                if notes.is_empty() { String::new() } else { format!("; {}", notes.join("; ")) },
                v["warnings"]
                    .as_array()
                    .filter(|w| !w.is_empty())
                    .map(|w| format!("\n  warnings: {}", w.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("; ")))
                    .unwrap_or_default()
            );
            Ok((t, v))
        }
        Command::Gguf { target, keys } => {
            let path = gguf_target(state, target).await?;
            let v = runner::read_gguf(&path.to_string_lossy(), keys)
                .await
                .map_err(ApiError::InvalidInput)?;
            let mut t = format!("{} (GGUF v{}, {} keys)\n", v["path"].as_str().unwrap_or(""), v["version"], v["key_count"]);
            for (k, x) in v["keys"].as_object().into_iter().flatten() {
                t.push_str(&format!("  {k} = {x}\n"));
            }
            Ok((t, v))
        }
        Command::History { limit } => {
            let runs = store::list_runs(&state.data_dir, *limit);
            let mut t = String::new();
            for r in &runs {
                t.push_str(&format!(
                    "  {}  {:<10} {}\n",
                    r["id"].as_str().unwrap_or(""),
                    r["status"].as_str().unwrap_or(""),
                    r["headline"].as_str().unwrap_or("")
                ));
            }
            if runs.is_empty() {
                t.push_str("no runs yet\n");
            }
            Ok((t, json!({ "runs": runs })))
        }
        Command::Show { run_id, full } => {
            let v = store::run_result(&state.data_dir, run_id, *full)?;
            let mut t = format!(
                "{} — {} ({})\n",
                run_id,
                v["status"].as_str().unwrap_or(""),
                v["summary"]["headline"].as_str().unwrap_or("")
            );
            if let Some(tbl) = v["summary"]["table"].as_str() {
                t.push_str(tbl);
            }
            for s in v["steps"].as_array().into_iter().flatten() {
                let verdict = s["verdict"].as_str().unwrap_or("");
                if verdict == "ok" || verdict == "skipped" {
                    continue;
                }
                if let Some(ex) = s["analysis"]["stderr_excerpt"].as_str().filter(|e| !e.is_empty()) {
                    t.push_str(&format!("\n[{}] {} — stderr excerpt:\n{}\n", s["index"].as_u64().unwrap_or(0) + 1, s["label"].as_str().unwrap_or(""), ex));
                }
            }
            Ok((t, v))
        }
        Command::Export { run_id } => {
            let v = store::export_run(&state.data_dir, run_id)?;
            Ok((format!("exported to {}", v["path"].as_str().unwrap_or("")), json!({ "path": v["path"] })))
        }
    }
}

/// A `gguf` target: a backend id (its model), a model id, or a path to
/// a `.gguf` file.
async fn gguf_target(state: &AppState, target: &str) -> Result<PathBuf, ApiError> {
    let cfg = state.config.read().await;
    if let Some(b) = cfg.backends.iter().find(|b| b.id == target) {
        return Ok(crate::resolve_under_data_dir(&cfg.data_dir, &b.model_file));
    }
    if let Some(m) = cfg.models.iter().find(|m| m.id == target) {
        return Ok(crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path));
    }
    let p = PathBuf::from(target);
    if p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
    {
        return Ok(if p.is_absolute() {
            p
        } else {
            cfg.data_dir.join("models").join(p)
        });
    }
    Err(ApiError::NotFound(format!(
        "{target}: not a backend id, model id or .gguf path"
    )))
}

/// Read the tail of `p` (at most `cap` bytes).
fn read_tail(p: &std::path::Path, cap: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    if len > cap {
        f.seek(SeekFrom::Start(len - cap)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A backend's persisted output: the current run (its overflow
/// segment, then the live file) or the previous run (`.prev`),
/// optionally grepped (case-insensitive), the last `tail` lines.
pub(crate) async fn backend_logs(
    state: &AppState,
    id: &str,
    prev: bool,
    grep: Option<&str>,
    tail: usize,
    stream: &str,
) -> ApiResult {
    if !state
        .config
        .read()
        .await
        .backends
        .iter()
        .any(|b| b.id == id)
    {
        return Err(ApiError::NotFound(format!("backend {id}")));
    }
    let base = match stream {
        "stderr" | "stdout" => crate::backend_output_log(&state.data_dir, id, stream),
        "lifecycle" => {
            crate::lifecycle::log_path(&state.data_dir, &crate::lifecycle::backend_key(id))
        }
        other => {
            return Err(ApiError::InvalidInput(format!(
                "stream {other:?}: expected stderr, stdout or lifecycle"
            )))
        }
    };
    let files: Vec<PathBuf> = if prev {
        vec![manager_supervisor::lifecycle::prev_log_path(&base)]
    } else {
        vec![manager_supervisor::overflow_log_path(&base), base.clone()]
    };
    let mut lines: Vec<String> = Vec::new();
    let mut present = Vec::new();
    for f in &files {
        if let Some(text) = read_tail(f, 32 << 20) {
            present.push(f.display().to_string());
            lines.extend(text.lines().map(str::to_string));
        }
    }
    let mut source = "file";
    if present.is_empty() && !prev && stream != "lifecycle" {
        // Nothing on disk (a backend started before output persisted):
        // the supervisor ring, if it has the run.
        let sup = state.supervisor().await.lock().await;
        let mut recs = sup.logs_query(id, None, None, 20_000).records;
        recs.reverse();
        let want = if stream == "stdout" {
            manager_supervisor::LogStream::Stdout
        } else {
            manager_supervisor::LogStream::Stderr
        };
        lines = recs
            .into_iter()
            .filter(|r| r.stream == want && !r.archived)
            .map(|r| r.line)
            .collect();
        source = "memory";
    }
    let total = lines.len();
    if let Some(g) = grep.filter(|g| !g.is_empty()) {
        let needle = g.to_lowercase();
        lines.retain(|l| l.to_lowercase().contains(&needle));
    }
    let matching = lines.len();
    let tail = tail.clamp(1, 5000);
    let skip = lines.len().saturating_sub(tail);
    let page: Vec<String> = lines.into_iter().skip(skip).collect();
    let header = format!(
        "{id} {stream} ({}{}): {} of {matching} matching line(s){}",
        if prev { "previous run" } else { "current run" },
        if source == "memory" {
            ", from memory"
        } else {
            ""
        },
        page.len(),
        grep.map(|g| format!(" for {g:?}")).unwrap_or_default()
    );
    let text = std::iter::once(header)
        .chain(page.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({
        "id": id,
        "stream": stream,
        "prev": prev,
        "files": present,
        "source": source,
        "total_lines": total,
        "matching": matching,
        "lines": page,
        "text": text,
    }))
}
