//! Executing a planned run.
//!
//! One run at a time. A run:
//!
//! 1. drains and stops the backends the plan names (the shared
//!    `backend_lifecycle` path: graceful `/slots` drain, operator
//!    intent, lifecycle markers with origin `console`);
//! 2. runs the steps one after another as supervised processes
//!    (`SupervisedProcess`: captured output, kill-on-drop, a bounded
//!    kill, the CUDA busy watchdog), streaming output live, sampling
//!    GPU telemetry, enforcing timeouts and cancellation;
//! 3. **always** restores the backends it stopped — after success, a
//!    crash, a timeout, a cancel, or a panic in the step task (the
//!    steps run in their own task, so the restore path survives it);
//! 4. persists everything under `diagnostics/runs/<id>/`.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manager_diag::recipe::{Expect, StepKind};
use manager_diag::summary::{self, Finished, StepAnalysis, Verdict};
use manager_supervisor::{LogRecord, LogStream, OutputLogs, RingBuffer, SupervisedProcess};
use serde_json::{json, Value};

use super::plan::{self, Plan, ResolvedStep};
use super::{audit, store, Caller, CONSOLE_OUTPUT_EVENT, CONSOLE_RUN_EVENT};
use crate::backend_lifecycle::{self, BackendOpOpts};
use crate::lifecycle::{self, backend_key, Origin};
use crate::registry::CapClass;
use crate::{ApiError, ApiResult, AppState, EventSink};

/// In-memory output lines kept per run (the file has all of them).
const LIVE_LINES: usize = 20_000;
/// Runs whose live buffers are kept.
const LIVE_RUNS: usize = 8;
/// Ring capacity for one step's capture.
const STEP_RING: usize = 50_000;
/// Output text kept in memory per stream for analysis (the tail).
const ANALYSIS_TEXT_CAP: usize = 8 * 1024 * 1024;
/// GPU telemetry sampling period during a step.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// Telemetry samples kept per step.
const MAX_SAMPLES: usize = 3600;
/// Poll period of the step loop.
const TICK: Duration = Duration::from_millis(100);
/// Leaf value the plan puts where the synthetic prompt goes.
pub(crate) const SYNTHETIC_SENTINEL: &str = "\u{1}synthetic_prompt\u{1}";

pub(crate) struct ActiveRun {
    pub run_id: String,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// A run's live output and status.
pub(crate) struct RunLive {
    pub id: String,
    buf: Mutex<OutBuf>,
    notify: tokio::sync::Notify,
    done: AtomicBool,
    status: Mutex<String>,
}

#[derive(Default)]
struct OutBuf {
    lines: VecDeque<Value>,
    next: u64,
}

impl RunLive {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            buf: Mutex::new(OutBuf {
                lines: VecDeque::new(),
                next: 1,
            }),
            notify: tokio::sync::Notify::new(),
            done: AtomicBool::new(false),
            status: Mutex::new("starting".into()),
        }
    }

    fn status(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, s: &str) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = s.to_string();
        self.notify.notify_waiters();
    }
}

pub(crate) struct StartRequest {
    pub recipe: manager_diag::Recipe,
    pub variant: Option<usize>,
    pub params: BTreeMap<String, String>,
    pub set: BTreeMap<String, String>,
    pub confirm_stop: Vec<String>,
    pub confirm_all: bool,
    pub restore: Option<bool>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Output sink for one run: memory buffer, `output.jsonl`, events.
struct Out {
    live: Arc<RunLive>,
    file: Option<std::fs::File>,
    sink: Arc<dyn EventSink>,
}

impl Out {
    fn push_many(&mut self, step: Option<usize>, lines: Vec<(&'static str, String)>) {
        if lines.is_empty() {
            return;
        }
        let ts = now_ms();
        let mut batch = Vec::with_capacity(lines.len());
        {
            let mut b = self.live.buf.lock().unwrap_or_else(|e| e.into_inner());
            for (stream, line) in lines {
                let rec = json!({ "seq": b.next, "ts_ms": ts, "step": step, "stream": stream, "line": line });
                b.next += 1;
                if let Some(f) = self.file.as_mut() {
                    let _ = f.write_all(
                        format!(
                            "{rec}
"
                        )
                        .as_bytes(),
                    );
                }
                b.lines.push_back(rec.clone());
                if b.lines.len() > LIVE_LINES {
                    b.lines.pop_front();
                }
                batch.push(rec);
            }
        }
        for chunk in batch.chunks(200) {
            self.sink.emit(
                CONSOLE_OUTPUT_EVENT,
                json!({ "run_id": self.live.id, "lines": chunk }),
            );
        }
        self.live.notify.notify_waiters();
    }

    fn console(&mut self, step: Option<usize>, line: impl Into<String>) {
        self.push_many(step, vec![("console", line.into())]);
    }
}

/// Live capture of one process: its ring, per-stream files, and the
/// analysis text (tail-capped).
struct Capture {
    ring: Arc<Mutex<RingBuffer<LogRecord>>>,
    seen: u64,
    step: usize,
    stdout_file: Option<std::fs::File>,
    stderr_file: Option<std::fs::File>,
    stdout: String,
    stderr: String,
}

fn cap_tail(s: &mut String) {
    if s.len() > ANALYSIS_TEXT_CAP {
        let mut cut = s.len() - ANALYSIS_TEXT_CAP / 2;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.replace_range(
            ..cut,
            "[… earlier output trimmed from the in-memory copy; the step log has it …]\n",
        );
    }
}

impl Capture {
    fn new(dir: &Path, step: usize) -> Self {
        let open = |name: String| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("steps").join(name))
                .ok()
        };
        Self {
            ring: Arc::new(Mutex::new(RingBuffer::new(STEP_RING))),
            seen: 0,
            step,
            stdout_file: open(format!("{step:02}.stdout.log")),
            stderr_file: open(format!("{step:02}.stderr.log")),
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn drain(&mut self, out: &mut Out) {
        let (fresh, dropped): (Vec<(LogStream, String)>, u64) = {
            let r = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            let pushed = r.pushed();
            let available = r.since(self.seen).count() as u64;
            let dropped = pushed.saturating_sub(self.seen).saturating_sub(available);
            let v = r
                .since(self.seen)
                .map(|rec| (rec.stream, rec.line.clone()))
                .collect();
            self.seen = pushed;
            (v, dropped)
        };
        let mut lines = Vec::with_capacity(fresh.len() + 1);
        if dropped > 0 {
            lines.push(("console", format!("[console] {dropped} output lines were produced faster than they could be read and are missing")));
        }
        for (stream, line) in fresh {
            let (name, file, text) = match stream {
                LogStream::Stdout => ("stdout", &mut self.stdout_file, &mut self.stdout),
                LogStream::Stderr => ("stderr", &mut self.stderr_file, &mut self.stderr),
            };
            if let Some(f) = file.as_mut() {
                let _ = writeln!(f, "{line}");
            }
            text.push_str(&line);
            text.push('\n');
            cap_tail(text);
            lines.push((name, line));
        }
        out.push_many(Some(self.step), lines);
    }
}

/// GPU telemetry for one step.
#[derive(Default)]
struct Telemetry {
    before: Value,
    samples: Vec<Value>,
    peak: BTreeMap<u32, (f32, u64)>,
    last: Option<Instant>,
}

async fn gpu_snapshot(state: &AppState) -> Value {
    let store = state.telemetry().await;
    let samples = crate::lock_telemetry(&store).snapshot();
    json!(samples
        .iter()
        .map(|g| json!({
            "index": g.index,
            "util_pct": g.utilization_pct,
            "vram_used_mib": g.vram_used_mib,
            "vram_total_mib": g.vram_total_mib,
            "stale": g.stale,
        }))
        .collect::<Vec<_>>())
}

impl Telemetry {
    async fn start(state: &AppState) -> Self {
        let before = gpu_snapshot(state).await;
        let mut t = Self {
            before: before.clone(),
            ..Default::default()
        };
        t.note(&before);
        t.last = Some(Instant::now());
        t
    }

    fn note(&mut self, snap: &Value) {
        for g in snap.as_array().into_iter().flatten() {
            let i = g["index"].as_u64().unwrap_or(0) as u32;
            let u = g["util_pct"].as_f64().unwrap_or(0.0) as f32;
            let m = g["vram_used_mib"].as_u64().unwrap_or(0);
            let e = self.peak.entry(i).or_insert((0.0, 0));
            e.0 = e.0.max(u);
            e.1 = e.1.max(m);
        }
    }

    async fn tick(&mut self, state: &AppState, started: Instant) {
        if self.last.is_some_and(|l| l.elapsed() < SAMPLE_EVERY) {
            return;
        }
        self.last = Some(Instant::now());
        let snap = gpu_snapshot(state).await;
        self.note(&snap);
        if self.samples.len() < MAX_SAMPLES {
            self.samples
                .push(json!({ "t_ms": started.elapsed().as_millis() as u64, "gpus": snap }));
        }
    }

    async fn finish(mut self, state: &AppState) -> Value {
        let after = gpu_snapshot(state).await;
        self.note(&after);
        json!({
            "before": self.before,
            "after": after,
            "peak": self.peak.iter().map(|(i, (u, m))| json!({ "index": i, "util_pct": u, "vram_used_mib": m })).collect::<Vec<_>>(),
            "samples": self.samples,
        })
    }
}

/// Shared, always-persistable run record.
struct Record {
    data_dir: PathBuf,
    id: String,
    doc: Value,
}

impl Record {
    fn save(&self) {
        if let Err(e) = store::write_run(&self.data_dir, &self.id, &self.doc) {
            tracing::warn!(run = %self.id, error = %e, "console run record write failed");
        }
    }
    fn step_mut(&mut self, i: usize) -> &mut Value {
        &mut self.doc["steps"][i]
    }
}

type SharedRecord = Arc<Mutex<Record>>;

fn with_rec<T>(rec: &SharedRecord, f: impl FnOnce(&mut Record) -> T) -> T {
    let mut r = rec.lock().unwrap_or_else(|e| e.into_inner());
    let out = f(&mut r);
    r.save();
    out
}

pub(crate) async fn active_id(state: &AppState) -> Option<String> {
    state
        .console
        .active
        .lock()
        .await
        .as_ref()
        .map(|a| a.run_id.clone())
}

/// Validate, confirm, and launch a run.
pub(crate) async fn start(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    caller: &Caller,
    req: StartRequest,
) -> ApiResult {
    super::require_exec(state, caller, "running a recipe").await?;
    caller.require_class(CapClass::D, "run recipes")?;
    let plan = plan::build_plan(state, &req.recipe, req.variant, &req.params, &req.set).await?;
    let commands: Vec<String> = plan.steps.iter().map(|s| s.command_line.clone()).collect();
    if !plan.errors.is_empty() {
        audit(
            state,
            caller,
            "console.run.start",
            Some(&req.recipe.name),
            "refused: plan errors",
            json!({ "errors": plan.errors, "commands": commands }),
        );
        return Ok(json!({
            "started": false,
            "reason": "the plan has errors; fix them and run again",
            "errors": plan.errors,
            "plan": plan.to_review_json(),
        }));
    }
    let unconfirmed: Vec<String> = plan
        .stop
        .iter()
        .map(|s| s.id.clone())
        .filter(|id| !req.confirm_all && !req.confirm_stop.contains(id))
        .collect();
    if !unconfirmed.is_empty() {
        return Ok(json!({
            "started": false,
            "needs_confirmation": true,
            "confirm_stop": unconfirmed,
            "reason": format!(
                "this run needs {} and must stop running backends first: {}. Re-run with confirm_stop (or --stop {} / --yes); they are restarted when the run ends.",
                if plan.gpus.is_empty() { "the named backends stopped".to_string() } else { plan.gpus.iter().map(|g| format!("GPU {g}")).collect::<Vec<_>>().join(", ") },
                unconfirmed.join(", "),
                unconfirmed.join(",")
            ),
            "plan": plan.to_review_json(),
        }));
    }
    if state.bench_job.lock().await.is_some() {
        return Err(ApiError::Busy(
            "a Bench sweep is running; wait for it or cancel it".into(),
        ));
    }
    let mut active = state.console.active.lock().await;
    if let Some(a) = active.as_ref() {
        return Err(ApiError::Busy(format!(
            "console run {} is still running (cancel it first)",
            a.run_id
        )));
    }
    let run_id = store::new_run_id(&state.data_dir, &req.recipe.name);
    let dir = store::run_dir(&state.data_dir, &run_id)?;
    std::fs::create_dir_all(dir.join("steps"))?;
    let restore = req.restore.unwrap_or(plan.restore);
    let steps_doc: Vec<Value> = plan
        .steps
        .iter()
        .map(|s| {
            json!({
                "index": s.index,
                "label": s.label,
                "step_index": s.step_index,
                "variant_index": s.variant_index,
                "variant_name": s.variant_name,
                "description": s.description,
                "kind": s.kind,
                "exe": s.exe,
                "args": s.args,
                "env": s.env,
                "workdir": s.workdir,
                "command_line": s.command_line,
                "expect": s.expect,
                "background": s.background,
                "gpus": s.gpus,
                "status": "pending",
                "stdout_file": format!("steps/{:02}.stdout.log", s.index),
                "stderr_file": format!("steps/{:02}.stderr.log", s.index),
            })
        })
        .collect();
    let doc = json!({
        "id": run_id,
        "status": "starting",
        "started_ms": now_ms(),
        "ended_ms": null,
        "recipe": plan.recipe,
        "variant": plan.variant,
        "params": plan.params,
        "set": plan.set,
        "caller": { "kind": caller.kind, "actor": caller.label(), "source_ip": caller.source_ip },
        "plan": {
            "gpus": plan.gpus,
            "gpu_reasons": plan.gpu_reasons,
            "declared_gpus": plan.declared_gpus,
            "stop_backends": plan.stop,
            "keep_running": plan.keep_running,
            "restore": restore,
            "warnings": plan.warnings,
        },
        "backends": { "stopped": [], "restored": [] },
        "steps": steps_doc,
        "summary": null,
        "error": null,
    });
    let record = Arc::new(Mutex::new(Record {
        data_dir: state.data_dir.clone(),
        id: run_id.clone(),
        doc,
    }));
    with_rec(&record, |_| {});
    let live = Arc::new(RunLive::new(&run_id));
    {
        let mut map = state.console.live.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= LIVE_RUNS {
            let mut ids: Vec<String> = map.keys().cloned().collect();
            ids.sort();
            for old in ids.iter().take(map.len() + 1 - LIVE_RUNS) {
                if map.get(old).is_some_and(|l| l.done.load(Ordering::SeqCst)) {
                    map.remove(old);
                }
            }
        }
        map.insert(run_id.clone(), Arc::clone(&live));
    }
    audit(
        state,
        caller,
        "console.run.start",
        Some(&req.recipe.name),
        "started",
        json!({
            "run_id": run_id,
            "variant": plan.variant,
            "params": plan.params,
            "commands": commands,
            "env": plan.steps.iter().map(|s| &s.env).collect::<Vec<_>>(),
            "stop_backends": plan.stop.iter().map(|s| &s.id).collect::<Vec<_>>(),
            "restore": restore,
        }),
    );
    let (tx, rx) = tokio::sync::watch::channel(false);
    *active = Some(ActiveRun {
        run_id: run_id.clone(),
        cancel: tx,
    });
    drop(active);
    let review = plan.to_review_json();
    let out_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("output.jsonl"))
        .ok();
    let out = Out {
        live: Arc::clone(&live),
        file: out_file,
        sink: Arc::clone(sink),
    };
    let (state2, sink2) = (Arc::clone(state), Arc::clone(sink));
    let actor = caller.label();
    tokio::spawn(async move {
        execute(state2, sink2, plan, restore, record, out, rx, dir, actor).await;
    });
    Ok(json!({
        "started": true,
        "run_id": run_id,
        "plan": review,
        "poll": format!("console_run_output run_id={run_id} since=<seq> (GET /api/v1/console/runs/{run_id}/output?since=0&wait_ms=20000)"),
    }))
}

/// Cancel the active run (it must be `run_id` when given).
pub(crate) async fn cancel(state: &AppState, run_id: Option<&str>) -> ApiResult {
    let active = state.console.active.lock().await;
    match active.as_ref() {
        Some(a) if run_id.is_none_or(|r| r == a.run_id) => {
            let _ = a.cancel.send(true);
            Ok(json!({ "cancelled": true, "run_id": a.run_id }))
        }
        Some(a) => Err(ApiError::InvalidInput(format!(
            "{} is not the active run ({} is)",
            run_id.unwrap_or(""),
            a.run_id
        ))),
        None => Err(ApiError::InvalidInput("no console run is active".into())),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    state: Arc<AppState>,
    sink: Arc<dyn EventSink>,
    plan: Plan,
    restore: bool,
    record: SharedRecord,
    mut out: Out,
    cancel: tokio::sync::watch::Receiver<bool>,
    dir: PathBuf,
    actor: String,
) {
    let run_id = out.live.id.clone();
    let emit_status = |sink: &Arc<dyn EventSink>, status: &str, extra: Value| {
        let mut v = json!({ "run_id": run_id, "status": status });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in e {
                o.insert(k.clone(), x.clone());
            }
        }
        sink.emit(CONSOLE_RUN_EVENT, v);
    };
    out.live.set_status("running");
    with_rec(&record, |r| r.doc["status"] = json!("running"));
    emit_status(&sink, "running", json!({}));
    out.console(
        None,
        format!(
            "[console] run {run_id} ({}) started by {actor}",
            plan.recipe.name
        ),
    );
    if !plan.gpus.is_empty() {
        out.console(
            None,
            format!(
                "[console] needs GPU(s) {:?}: {}",
                plan.gpus,
                plan.gpu_reasons.join("; ")
            ),
        );
    }

    // 1. Drain and stop the conflicting backends.
    let mut stopped: Vec<String> = Vec::new();
    let mut abort: Option<String> = None;
    for c in &plan.stop {
        if *cancel.borrow() {
            abort = Some("cancelled before the steps started".into());
            break;
        }
        let key = backend_key(&c.id);
        lifecycle::mark(
            &state.data_dir,
            &key,
            "console: draining and stopping for a diagnostics run",
            Some(&format!(
                "run {run_id} ({}): {}",
                plan.recipe.name, c.reason
            )),
            Some(Origin::Console),
        );
        out.console(
            None,
            format!(
                "[console] draining and stopping backend {} ({})…",
                c.id, c.reason
            ),
        );
        let opts = BackendOpOpts {
            graceful: true,
            ..BackendOpOpts::new(Origin::Console)
        };
        let r = backend_lifecycle::stop_backend(&state, &sink, &c.id, opts).await;
        let entry = json!({ "id": c.id, "reason": c.reason, "ok": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string()), "at_ms": now_ms() });
        with_rec(&record, |rec| {
            if let Some(a) = rec.doc["backends"]["stopped"].as_array_mut() {
                a.push(entry)
            }
        });
        match r {
            Ok(_) => {
                out.console(None, format!("[console] backend {} stopped", c.id));
                stopped.push(c.id.clone());
            }
            Err(e) => {
                out.console(
                    None,
                    format!(
                        "[console] stopping backend {} failed: {e}; the steps will not run",
                        c.id
                    ),
                );
                abort = Some(format!("stopping backend {} failed: {e}", c.id));
                break;
            }
        }
    }

    // 2. The steps, in their own task: a panic there cannot skip the
    //    restore below.
    let mut status = "completed".to_string();
    if let Some(reason) = &abort {
        status = if reason.starts_with("cancelled") {
            "cancelled".into()
        } else {
            "failed".into()
        };
        with_rec(&record, |r| r.doc["error"] = json!(reason));
        mark_remaining_skipped(&record, 0);
    } else {
        let task = {
            let state = Arc::clone(&state);
            let record = Arc::clone(&record);
            let steps = plan.steps.clone();
            let dir = dir.clone();
            let cancel = cancel.clone();
            let live = Arc::clone(&out.live);
            let sink = Arc::clone(&out.sink);
            tokio::spawn(async move {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("output.jsonl"))
                    .ok();
                let mut sout = Out { live, file, sink };
                run_steps(&state, &steps, &record, &mut sout, cancel, &dir).await
            })
        };
        match task.await {
            Ok(StepsEnd::Done) => {}
            Ok(StepsEnd::Cancelled) => status = "cancelled".into(),
            // Every step ran and its evidence is recorded; at least
            // one that was expected to succeed did not.
            Ok(StepsEnd::Failed) => status = "completed_with_failures".into(),
            Err(e) => {
                status = "failed".into();
                let msg = format!("the step runner stopped unexpectedly: {e}");
                out.console(None, format!("[console] {msg}"));
                with_rec(&record, |r| r.doc["error"] = json!(msg));
            }
        }
    }

    // 3. Restore — whatever happened above.
    if restore {
        for id in &stopped {
            let key = backend_key(id);
            lifecycle::mark(
                &state.data_dir,
                &key,
                "console: restarting after a diagnostics run",
                Some(&format!("run {run_id} ended {status}")),
                Some(Origin::Console),
            );
            out.console(None, format!("[console] restarting backend {id}…"));
            let r = backend_lifecycle::start_backend(
                &state,
                &sink,
                id,
                BackendOpOpts::new(Origin::Console),
            )
            .await;
            match &r {
                Ok(_) => out.console(None, format!("[console] backend {id} start requested (readiness is watched in the background)")),
                Err(e) => out.console(None, format!("[console] restarting backend {id} FAILED: {e} — start it manually")),
            }
            let entry = json!({ "id": id, "ok": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string()), "at_ms": now_ms() });
            with_rec(&record, |rec| {
                if let Some(a) = rec.doc["backends"]["restored"].as_array_mut() {
                    a.push(entry)
                }
            });
        }
    } else if !stopped.is_empty() {
        out.console(
            None,
            format!(
                "[console] leaving {} stopped (restore is off)",
                stopped.join(", ")
            ),
        );
    }

    // 4. Summary and the final record.
    let summary = with_rec(&record, |r| {
        let s = build_summary(&r.doc);
        r.doc["summary"] = s.clone();
        r.doc["status"] = json!(status);
        r.doc["ended_ms"] = json!(now_ms());
        s
    });
    out.console(
        None,
        format!(
            "[console] run {run_id} {status}: {}",
            summary["headline"].as_str().unwrap_or("")
        ),
    );
    if let Some(t) = summary["table"].as_str() {
        out.push_many(
            None,
            t.lines().map(|l| ("console", l.to_string())).collect(),
        );
    }
    {
        let mut active = state.console.active.lock().await;
        if active.as_ref().is_some_and(|a| a.run_id == run_id) {
            *active = None;
        }
    }
    out.live.done.store(true, Ordering::SeqCst);
    out.live.set_status(&status);
    emit_status(&sink, &status, json!({ "headline": summary["headline"] }));
}

enum StepsEnd {
    Done,
    Cancelled,
    Failed,
}

fn mark_remaining_skipped(record: &SharedRecord, from: usize) {
    with_rec(record, |r| {
        if let Some(steps) = r.doc["steps"].as_array_mut() {
            for s in steps.iter_mut().skip(from) {
                if s["status"] == "pending" {
                    s["status"] = json!("skipped");
                    s["verdict"] = json!(Verdict::Skipped);
                }
            }
        }
    });
}

/// A background step still running.
struct Bg {
    step: usize,
    proc: SupervisedProcess,
    cap: Capture,
    tel: Telemetry,
    started: Instant,
    started_ms: i64,
    spec: ResolvedStep,
}

fn exit_parts(status: &std::process::ExitStatus) -> (Option<i64>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code().map(i64::from), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code().map(i64::from), None)
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_step(
    record: &SharedRecord,
    out: &mut Out,
    s: &ResolvedStep,
    cap: &Capture,
    exit: Option<(Option<i64>, Option<i32>)>,
    timed_out: bool,
    cancelled: bool,
    stopped_by_runner: bool,
    telemetry: Value,
    started_ms: i64,
    error: Option<String>,
) -> Verdict {
    let (code, sig) = exit.unwrap_or((None, None));
    let mut analysis: StepAnalysis = summary::analyze(&Finished {
        exit_code: code,
        signal: sig,
        timed_out,
        cancelled,
        stdout: &cap.stdout,
        stderr: &cap.stderr,
        parser: s.parser,
        expect: s.expect,
        args: &s.args,
    });
    if stopped_by_runner && !cancelled {
        // The runner ended a background server at the end of the
        // recipe: that kill is not the server's failure.
        analysis.verdict = Verdict::Ok;
    }
    let verdict = analysis.verdict;
    let line = format!(
        "[console] step {} {}: {}{}",
        s.index + 1,
        s.label,
        match verdict {
            Verdict::Ok => "ok",
            Verdict::ExpectedFailure => "crashed/failed as allowed (recorded as evidence)",
            Verdict::Failed => "FAILED",
            Verdict::TimedOut => "TIMED OUT",
            Verdict::Cancelled => "cancelled",
            Verdict::Skipped => "skipped",
        },
        analysis
            .exit
            .as_ref()
            .map(|e| format!(" — exit {} ({})", e.display, e.meaning))
            .unwrap_or_default()
    );
    out.console(Some(s.index), line);
    let ended = now_ms();
    with_rec(record, |r| {
        let st = r.step_mut(s.index);
        st["status"] = json!("done");
        st["verdict"] = json!(verdict);
        st["exit"] = json!(analysis.exit);
        st["analysis"] = json!(analysis);
        st["telemetry"] = telemetry;
        st["started_ms"] = json!(started_ms);
        st["ended_ms"] = json!(ended);
        st["duration_ms"] = json!(ended - started_ms);
        st["timed_out"] = json!(timed_out);
        st["stopped_by_runner"] = json!(stopped_by_runner);
        if let Some(e) = error {
            st["error"] = json!(e);
        }
    });
    verdict
}

/// Pump background captures; reap any that exited by themselves.
async fn pump_bg(state: &AppState, bgs: &mut Vec<Bg>, record: &SharedRecord, out: &mut Out) {
    let mut i = 0;
    while i < bgs.len() {
        bgs[i].cap.drain(out);
        let started = bgs[i].started;
        bgs[i].tel.tick(state, started).await;
        let exited = bgs[i]
            .proc
            .poll_exited_drained(manager_supervisor::READER_DRAIN_GRACE)
            .await
            .ok()
            .flatten();
        if let Some(st) = exited {
            let mut b = bgs.remove(i);
            b.cap.drain(out);
            out.console(
                Some(b.step),
                format!(
                    "[console] background step {} exited on its own",
                    b.spec.label
                ),
            );
            let tel = b.tel.finish(state).await;
            finish_step(
                record,
                out,
                &b.spec,
                &b.cap,
                Some(exit_parts(&st)),
                false,
                false,
                false,
                tel,
                b.started_ms,
                None,
            );
            continue;
        }
        i += 1;
    }
}

async fn stop_bgs(
    state: &AppState,
    bgs: Vec<Bg>,
    record: &SharedRecord,
    out: &mut Out,
    cancelled: bool,
) {
    for mut b in bgs {
        out.console(
            Some(b.step),
            format!("[console] stopping background step {}", b.spec.label),
        );
        let natural = b
            .proc
            .poll_exited_drained(manager_supervisor::READER_DRAIN_GRACE)
            .await
            .ok()
            .flatten();
        let (status, by_runner) = match natural {
            Some(st) => (Some(st), false),
            None => {
                let _ = b.proc.kill().await;
                (b.proc.last_exit(), true)
            }
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        b.cap.drain(out);
        let tel = b.tel.finish(state).await;
        finish_step(
            record,
            out,
            &b.spec,
            &b.cap,
            status.as_ref().map(exit_parts),
            false,
            cancelled && by_runner,
            by_runner,
            tel,
            b.started_ms,
            None,
        );
    }
}

fn env_vec(s: &ResolvedStep) -> Vec<(String, String)> {
    s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

async fn spawn_step(
    s: &ResolvedStep,
    run_id_tag: &str,
    cap: &Capture,
) -> Result<SupervisedProcess, String> {
    let exe = s.exe.as_deref().ok_or("no resolved executable")?;
    SupervisedProcess::spawn_with_logs(
        &format!("console-{run_id_tag}-{}", s.index),
        Path::new(exe),
        &s.args,
        &env_vec(s),
        s.workdir.as_deref().map(Path::new),
        Arc::clone(&cap.ring),
        true,
        &OutputLogs::default(),
    )
    .await
    .map_err(|e| e.to_string())
}

async fn probe_ready(url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    matches!(client.get(url).send().await, Ok(r) if r.status().is_success())
}

async fn run_steps(
    state: &AppState,
    steps: &[ResolvedStep],
    record: &SharedRecord,
    out: &mut Out,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    dir: &Path,
) -> StepsEnd {
    let tag: String = out
        .live
        .id
        .chars()
        .rev()
        .take(12)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let mut bgs: Vec<Bg> = Vec::new();
    let mut end = StepsEnd::Done;
    for (pos, s) in steps.iter().enumerate() {
        if *cancel.borrow() {
            end = StepsEnd::Cancelled;
            mark_remaining_skipped(record, pos);
            break;
        }
        out.console(
            Some(s.index),
            format!(
                "[console] step {}/{}: {} — {}",
                s.index + 1,
                steps.len(),
                s.label,
                s.command_line
            ),
        );
        if !s.description.is_empty() {
            out.console(Some(s.index), format!("[console]   {}", s.description));
        }
        let started_ms = now_ms();
        with_rec(record, |r| {
            let st = r.step_mut(s.index);
            st["status"] = json!("running");
            st["started_ms"] = json!(started_ms);
        });
        let verdict = match s.kind {
            StepKind::Exec => {
                let mut cap = Capture::new(dir, s.index);
                let mut tel = Telemetry::start(state).await;
                let started = Instant::now();
                let mut proc = match spawn_step(s, &tag, &cap).await {
                    Ok(p) => p,
                    Err(e) => {
                        out.console(Some(s.index), format!("[console] spawn failed: {e}"));
                        let tel = tel.finish(state).await;
                        let v = finish_step(
                            record,
                            out,
                            s,
                            &cap,
                            None,
                            false,
                            false,
                            false,
                            tel,
                            started_ms,
                            Some(e),
                        );
                        if v == Verdict::Failed && s.expect == Expect::Success {
                            end = StepsEnd::Failed;
                        }
                        continue;
                    }
                };
                if s.background {
                    // Wait for readiness, pumping output.
                    let deadline = started + Duration::from_secs(s.ready_timeout_secs.max(1));
                    let mut ready = s.ready_url.is_none();
                    let mut exited = None;
                    let mut last_probe = Instant::now() - Duration::from_secs(5);
                    while !ready {
                        cap.drain(out);
                        tel.tick(state, started).await;
                        pump_bg(state, &mut bgs, record, out).await;
                        if let Ok(Some(st)) = proc
                            .poll_exited_drained(manager_supervisor::READER_DRAIN_GRACE)
                            .await
                        {
                            exited = Some(st);
                            break;
                        }
                        if *cancel.borrow() || Instant::now() >= deadline {
                            break;
                        }
                        if last_probe.elapsed() >= Duration::from_millis(500) {
                            last_probe = Instant::now();
                            if let Some(u) = &s.ready_url {
                                ready = probe_ready(u).await;
                            }
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(TICK) => {}
                            _ = cancel.changed() => {}
                        }
                    }
                    cap.drain(out);
                    if ready {
                        out.console(
                            Some(s.index),
                            format!("[console] background step {} is ready", s.label),
                        );
                        with_rec(record, |r| r.step_mut(s.index)["status"] = json!("ready"));
                        bgs.push(Bg {
                            step: s.index,
                            proc,
                            cap,
                            tel,
                            started,
                            started_ms,
                            spec: s.clone(),
                        });
                        continue;
                    }
                    let cancelled = *cancel.borrow();
                    let timed_out = exited.is_none() && !cancelled;
                    if exited.is_none() {
                        let _ = proc.kill().await;
                    }
                    let tel = tel.finish(state).await;
                    let st = exited.or(proc.last_exit());
                    finish_step(
                        record,
                        out,
                        s,
                        &cap,
                        st.as_ref().map(exit_parts),
                        timed_out,
                        cancelled,
                        false,
                        tel,
                        started_ms,
                        timed_out.then(|| "never became ready".to_string()),
                    )
                } else {
                    let deadline = started + Duration::from_secs(s.timeout_secs.max(1));
                    let mut timed_out = false;
                    let mut cancelled = false;
                    let status = loop {
                        cap.drain(out);
                        tel.tick(state, started).await;
                        pump_bg(state, &mut bgs, record, out).await;
                        match proc
                            .poll_exited_drained(manager_supervisor::READER_DRAIN_GRACE)
                            .await
                        {
                            Ok(Some(st)) => break Some(st),
                            Ok(None) => {}
                            Err(e) => {
                                out.console(Some(s.index), format!("[console] wait failed: {e}"));
                                let _ = proc.kill().await;
                                break proc.last_exit();
                            }
                        }
                        if *cancel.borrow() {
                            cancelled = true;
                            out.console(Some(s.index), "[console] cancelling: killing the step");
                            let _ = proc.kill().await;
                            break proc.last_exit();
                        }
                        if Instant::now() >= deadline {
                            timed_out = true;
                            out.console(
                                Some(s.index),
                                format!(
                                    "[console] timeout after {} s: killing the step",
                                    s.timeout_secs
                                ),
                            );
                            let _ = proc.kill().await;
                            break proc.last_exit();
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(TICK) => {}
                            _ = cancel.changed() => {}
                        }
                    };
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    cap.drain(out);
                    let tel = tel.finish(state).await;
                    let exit = if cancelled || timed_out {
                        None
                    } else {
                        status.as_ref().map(exit_parts)
                    };
                    finish_step(
                        record, out, s, &cap, exit, timed_out, cancelled, false, tel, started_ms,
                        None,
                    )
                }
            }
            StepKind::Http => {
                run_http(state, s, record, out, &mut cancel, &mut bgs, started_ms).await
            }
            StepKind::Gguf => run_gguf(state, s, record, out, started_ms).await,
        };
        match verdict {
            Verdict::Cancelled => {
                end = StepsEnd::Cancelled;
                mark_remaining_skipped(record, pos + 1);
                break;
            }
            Verdict::Failed | Verdict::TimedOut if s.expect == Expect::Success => {
                // A failing step that was expected to succeed is
                // recorded; later steps still run (evidence from the
                // other variants is the point of an A/B).
                end = StepsEnd::Failed;
            }
            _ => {}
        }
    }
    let cancelled = *cancel.borrow();
    stop_bgs(state, std::mem::take(&mut bgs), record, out, cancelled).await;
    if cancelled {
        mark_remaining_skipped(record, 0);
        return StepsEnd::Cancelled;
    }
    end
}

/// About `tokens` tokens of varied, deterministic English-like text.
pub(crate) fn synthetic_prompt(tokens: u32) -> String {
    const WORDS: &[&str] = &[
        "the",
        "system",
        "records",
        "each",
        "measurement",
        "under",
        "steady",
        "load",
        "while",
        "the",
        "cache",
        "grows",
        "with",
        "every",
        "request",
        "and",
        "operators",
        "compare",
        "results",
        "across",
        "runs",
        "noting",
        "latency",
        "memory",
        "pressure",
        "throughput",
        "context",
        "depth",
        "kernel",
        "launch",
    ];
    let mut s = String::with_capacity(tokens as usize * 7);
    s.push_str("Summarize the following log of observations in one sentence.\n");
    let mut n = 0u32;
    let mut sentence = 0u32;
    while n < tokens {
        sentence += 1;
        s.push_str(&format!("Observation {sentence}:"));
        n += 3;
        for k in 0..12 {
            s.push(' ');
            s.push_str(WORDS[((sentence * 7 + k * 3) as usize) % WORDS.len()]);
            n += 1;
        }
        s.push_str(".\n");
        n += 1;
    }
    s
}

fn fill_synthetic(v: &Value, prompt: &str) -> Value {
    match v {
        Value::String(s) if s.contains(SYNTHETIC_SENTINEL) => {
            Value::String(s.replace(SYNTHETIC_SENTINEL, prompt))
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_synthetic(x, prompt)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), fill_synthetic(x, prompt)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_http(
    state: &AppState,
    s: &ResolvedStep,
    record: &SharedRecord,
    out: &mut Out,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    bgs: &mut Vec<Bg>,
    started_ms: i64,
) -> Verdict {
    let h = s.http.as_ref().expect("planned http step");
    let started = Instant::now();
    let mut body = h.body.clone();
    if let Some(n) = h.synthetic_prompt_tokens {
        let prompt = synthetic_prompt(n);
        out.console(
            Some(s.index),
            format!(
                "[console] synthetic prompt: ~{n} tokens ({} chars)",
                prompt.len()
            ),
        );
        body = body.map(|b| fill_synthetic(&b, &prompt));
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(s.timeout_secs.max(1)))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return http_done(
                record,
                out,
                s,
                started_ms,
                started,
                None,
                None,
                Some(e.to_string()),
                false,
            )
        }
    };
    let method = reqwest::Method::from_bytes(h.method.as_bytes()).unwrap_or(reqwest::Method::POST);
    let mut rq = client.request(method, &h.url);
    if let Some(b) = &body {
        rq = rq
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(b).unwrap_or_default());
    }
    let fut = async move {
        let r = rq.send().await?;
        let code = r.status().as_u16();
        let text = r.text().await?;
        Ok::<(u16, String), reqwest::Error>((code, text))
    };
    tokio::pin!(fut);
    let result = loop {
        tokio::select! {
            r = &mut fut => break Some(r),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                pump_bg(state, bgs, record, out).await;
                if *cancel.borrow() { break None; }
            }
            _ = cancel.changed() => { if *cancel.borrow() { break None; } }
        }
    };
    match result {
        None => http_done(record, out, s, started_ms, started, None, None, None, true),
        Some(Ok((code, text))) => http_done(
            record,
            out,
            s,
            started_ms,
            started,
            Some(code),
            Some(text),
            None,
            false,
        ),
        Some(Err(e)) => {
            // Give a crashing server a moment to be reaped so its exit
            // is in the record next to this failure.
            tokio::time::sleep(Duration::from_millis(500)).await;
            pump_bg(state, bgs, record, out).await;
            http_done(
                record,
                out,
                s,
                started_ms,
                started,
                None,
                None,
                Some(e.to_string()),
                false,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn http_done(
    record: &SharedRecord,
    out: &mut Out,
    s: &ResolvedStep,
    started_ms: i64,
    started: Instant,
    code: Option<u16>,
    text: Option<String>,
    error: Option<String>,
    cancelled: bool,
) -> Verdict {
    let ok = code.is_some_and(|c| (200..300).contains(&c));
    let verdict = if cancelled {
        Verdict::Cancelled
    } else if ok {
        Verdict::Ok
    } else if matches!(s.expect, Expect::MayFail | Expect::MayCrash) {
        Verdict::ExpectedFailure
    } else {
        Verdict::Failed
    };
    let parsed: Option<Value> = text.as_deref().and_then(|t| serde_json::from_str(t).ok());
    let excerpt: Option<String> = text.as_deref().map(|t| t.chars().take(2000).collect());
    out.console(
        Some(s.index),
        format!(
            "[console] step {} {}: {}",
            s.index + 1,
            s.label,
            match (&code, &error) {
                (Some(c), _) => format!("HTTP {c} in {} ms", started.elapsed().as_millis()),
                (None, Some(e)) => format!("request failed: {e}"),
                _ => "cancelled".to_string(),
            }
        ),
    );
    let ended = now_ms();
    with_rec(record, |r| {
        let st = r.step_mut(s.index);
        st["status"] = json!("done");
        st["verdict"] = json!(verdict);
        st["started_ms"] = json!(started_ms);
        st["ended_ms"] = json!(ended);
        st["duration_ms"] = json!(ended - started_ms);
        st["http"] = json!({
            "status": code,
            "error": error,
            "timings": parsed.as_ref().and_then(|p| p.get("timings")).cloned(),
            "response_excerpt": excerpt,
        });
    });
    verdict
}

async fn run_gguf(
    state: &AppState,
    s: &ResolvedStep,
    record: &SharedRecord,
    out: &mut Out,
    started_ms: i64,
) -> Verdict {
    let g = s.gguf.as_ref().expect("planned gguf step");
    let r = read_gguf(&g.path, &g.keys).await;
    let ended = now_ms();
    let _ = state;
    let (verdict, val) = match r {
        Ok(v) => {
            let lines: Vec<(&'static str, String)> = v["keys"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, x)| ("stdout", format!("{k} = {x}")))
                        .collect()
                })
                .unwrap_or_default();
            out.push_many(Some(s.index), lines);
            (Verdict::Ok, v)
        }
        Err(e) => {
            out.console(Some(s.index), format!("[console] gguf read failed: {e}"));
            (Verdict::Failed, json!({ "error": e }))
        }
    };
    with_rec(record, |r| {
        let st = r.step_mut(s.index);
        st["status"] = json!("done");
        st["verdict"] = json!(verdict);
        st["started_ms"] = json!(started_ms);
        st["ended_ms"] = json!(ended);
        st["duration_ms"] = json!(ended - started_ms);
        st["gguf"] = val;
    });
    verdict
}

/// Read a GGUF file's metadata, growing the prefix as needed (up to
/// 512 MiB: the tokenizer arrays of large vocabularies are big).
pub(crate) async fn read_gguf(path: &str, keys: &[String]) -> Result<Value, String> {
    let p = PathBuf::from(path);
    if !p
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
    {
        return Err(format!("{path}: not a .gguf file"));
    }
    let path2 = p.clone();
    let keys = keys.to_vec();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&path2).map_err(|e| format!("{}: {e}", path2.display()))?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut buf: Vec<u8> = Vec::new();
        let mut want: u64 = 8 << 20;
        loop {
            let target = want.min(len).max(buf.len() as u64);
            if target > buf.len() as u64 {
                let mut chunk = vec![0u8; (target - buf.len() as u64) as usize];
                f.read_exact(&mut chunk).map_err(|e| e.to_string())?;
                buf.extend_from_slice(&chunk);
            }
            match manager_diag::gguf::read_metadata(&buf) {
                Ok(m) => {
                    let filtered = manager_diag::gguf::filter_keys(&m, &keys);
                    return Ok(json!({
                        "path": path2.display().to_string(),
                        "version": m.version,
                        "tensor_count": m.tensor_count,
                        "key_count": m.kv.len(),
                        "keys": filtered,
                    }));
                }
                Err(manager_diag::gguf::GgufMetaError::Incomplete)
                    if (buf.len() as u64) < len && want < (512 << 20) =>
                {
                    want *= 4;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The results table and headline from a run record.
fn build_summary(doc: &Value) -> Value {
    let mut rows = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for s in doc["steps"].as_array().into_iter().flatten() {
        let verdict: Verdict =
            serde_json::from_value(s["verdict"].clone()).unwrap_or(Verdict::Skipped);
        *counts
            .entry(
                serde_json::to_value(verdict)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            )
            .or_default() += 1;
        let label = s["variant_name"]
            .as_str()
            .map(|v| {
                format!(
                    "{} [{}]",
                    v,
                    s["variant_index"].as_u64().map(|i| i + 1).unwrap_or(0)
                )
            })
            .unwrap_or_else(|| s["label"].as_str().unwrap_or("").to_string());
        let row = match serde_json::from_value::<StepAnalysis>(s["analysis"].clone()) {
            Ok(a) => summary::row(&label, &a),
            Err(_) => summary::Row {
                variant: label,
                verdict,
                exit: s["http"]["status"]
                    .as_u64()
                    .map(|c| format!("HTTP {c}"))
                    .unwrap_or_else(|| match verdict {
                        Verdict::Skipped => "skipped".into(),
                        Verdict::Cancelled => "cancelled".into(),
                        _ => "—".into(),
                    }),
                decoded: s["http"]["error"].as_str().unwrap_or("").to_string(),
                pp_ts_per_depth: BTreeMap::new(),
                failing_depth: None,
                cuda_error: String::new(),
            },
        };
        rows.push(row);
    }
    let headline = format!(
        "{} execution(s): {}",
        rows.len(),
        counts
            .iter()
            .map(|(k, v)| format!("{v} {}", k.replace('_', " ")))
            .collect::<Vec<_>>()
            .join(", ")
    );
    json!({
        "headline": headline,
        "table": summary::table(&rows),
        "rows": rows,
    })
}

/// Output after `since` for one run, live or from disk.
pub(crate) async fn output(
    state: &AppState,
    run_id: &str,
    since: u64,
    wait_ms: Option<u64>,
    limit: Option<u64>,
) -> ApiResult {
    let limit = limit.unwrap_or(500).clamp(1, 5000) as usize;
    let live = state
        .console
        .live
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run_id)
        .cloned();
    if let Some(live) = live {
        let wait = Duration::from_millis(wait_ms.unwrap_or(0).min(25_000));
        let deadline = Instant::now() + wait;
        loop {
            let notified = live.notify.notified();
            let (lines, next, first) = {
                let b = live.buf.lock().unwrap_or_else(|e| e.into_inner());
                let lines: Vec<Value> = b
                    .lines
                    .iter()
                    .filter(|l| l["seq"].as_u64().unwrap_or(0) > since)
                    .take(limit)
                    .cloned()
                    .collect();
                let first = b.lines.front().and_then(|l| l["seq"].as_u64());
                (lines, b.next - 1, first)
            };
            let done = live.done.load(Ordering::SeqCst);
            if !lines.is_empty() || done || Instant::now() >= deadline {
                let last = lines
                    .last()
                    .and_then(|l| l["seq"].as_u64())
                    .unwrap_or(since.min(next));
                return Ok(json!({
                    "run_id": run_id,
                    "status": live.status(),
                    "done": done,
                    "lines": lines,
                    "next": last,
                    "latest": next,
                    "first_in_memory": first,
                    "more": last < next,
                }));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }
    // Finished and no longer in memory: from disk.
    let rec = store::read_run(&state.data_dir, run_id)?;
    let dir = store::run_dir(&state.data_dir, run_id)?;
    let text = std::fs::read_to_string(dir.join("output.jsonl")).unwrap_or_default();
    let mut latest = 0;
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .inspect(|v| latest = latest.max(v["seq"].as_u64().unwrap_or(0)))
        .filter(|v| v["seq"].as_u64().unwrap_or(0) > since)
        .take(limit)
        .collect();
    let last = lines
        .last()
        .and_then(|l| l["seq"].as_u64())
        .unwrap_or(since);
    Ok(json!({
        "run_id": run_id,
        "status": rec["status"],
        "done": rec["status"] != "running" && rec["status"] != "starting",
        "lines": lines,
        "next": last,
        "latest": latest,
        "more": last < latest,
    }))
}
