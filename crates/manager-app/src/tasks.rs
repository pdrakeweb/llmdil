//! Cooperative cancellation for the task registry (`tasks_list` /
//! `task_abort`).
//!
//! Every task recorded with `AppState::record_task` gets a
//! [`CancellationToken`] and a "finished" signal here. `task_abort`
//! no longer just flips the record:
//!
//! 1. it cancels the task's token, which long-running work observes at
//!    its natural checkpoints (the wizard runner stops the running step
//!    and starts no further one; see `Wizard::run_step_until`);
//! 2. when the operation already owns a cancel mechanism (a download's
//!    `cancel_download`, `bench_run_cancel`,
//!    `bench_prompt_generate_cancel`, `optimize_cancel_consult`,
//!    `optimize_cancel_job`), the abort routes to that mechanism — the
//!    operation registers it with [`TaskControls::set_canceller`] once
//!    it owns the slot the cancel op fires, so the abort never cancels
//!    a different job;
//! 3. it waits up to a bounded grace period for the work to finish.
//!    Work that stops records its own terminal state — a non-success
//!    finish after an abort request is recorded `aborted`. Work that
//!    does not stop in time is marked `aborted` with an `abort_note`
//!    saying it may still be running; its eventual outcome is kept as
//!    `late_outcome` in the detail instead of overwriting the abort.
//!
//! Aborting a task that already reached a terminal state is a no-op:
//! the record keeps its real status (it used to be overwritten with
//! `aborted`, claiming an effect that never happened).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{ApiError, ApiResult, AppState, EventSink, TaskRecord, TaskStatus};

/// How long `task_abort` waits for cancelled work to stop before it
/// marks the task aborted regardless.
pub(crate) const DEFAULT_ABORT_GRACE: Duration = Duration::from_secs(10);

/// An operation's own cancel mechanism, which `task_abort` routes to
/// instead of duplicating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskCanceller {
    /// `cancel_download` for this model (the worker removes the
    /// partial file, as an operator cancel always has).
    Download { model_id: String },
    /// `bench_run_cancel` for this sweep.
    BenchRun { sweep_id: String },
    /// `bench_prompt_generate_cancel`.
    BenchPromptGenerate,
    /// `optimize_cancel_consult`.
    OptimizeConsult,
    /// `optimize_cancel_job` (measurement, estimate, screening eval,
    /// experiment batch, tuning sweep — they share one job slot).
    OptimizeJob,
}

impl TaskCanceller {
    /// The registry op the abort routes to.
    pub(crate) fn op_name(&self) -> &'static str {
        match self {
            TaskCanceller::Download { .. } => "cancel_download",
            TaskCanceller::BenchRun { .. } => "bench_run_cancel",
            TaskCanceller::BenchPromptGenerate => "bench_prompt_generate_cancel",
            TaskCanceller::OptimizeConsult => "optimize_cancel_consult",
            TaskCanceller::OptimizeJob => "optimize_cancel_job",
        }
    }

    async fn invoke(&self, state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
        match self {
            TaskCanceller::Download { model_id } => {
                crate::op_cancel_download(state, sink, model_id.clone()).await
            }
            TaskCanceller::BenchRun { sweep_id } => {
                crate::bench::op_bench_run_cancel(state, sink, sweep_id.clone()).await
            }
            TaskCanceller::BenchPromptGenerate => {
                crate::bench::op_bench_prompt_generate_cancel(state, sink).await
            }
            TaskCanceller::OptimizeConsult => {
                crate::optimize::op_optimize_cancel_consult(state, sink).await
            }
            TaskCanceller::OptimizeJob => {
                crate::optimize::op_optimize_cancel_job(state, sink).await
            }
        }
    }
}

struct Control {
    token: CancellationToken,
    canceller: Option<TaskCanceller>,
    finished: watch::Sender<bool>,
    abort_requested: bool,
}

/// Per-task cancellation state, keyed by task id. Entries live from
/// `record_task` until the task's finish.
#[derive(Default)]
pub(crate) struct TaskControls {
    inner: std::sync::Mutex<HashMap<String, Control>>,
    /// Abort grace in ms; 0 means [`DEFAULT_ABORT_GRACE`].
    grace_ms: AtomicU64,
}

impl TaskControls {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Control>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Create the control for a new task.
    pub(crate) fn register(&self, id: &str) {
        let (finished, _) = watch::channel(false);
        self.lock().insert(
            id.to_string(),
            Control {
                token: CancellationToken::new(),
                canceller: None,
                finished,
                abort_requested: false,
            },
        );
    }

    /// The task's cancellation token. A task without a control (not
    /// recorded, or already finished) gets a fresh token that never
    /// fires.
    pub(crate) fn token(&self, id: &str) -> CancellationToken {
        self.lock()
            .get(id)
            .map(|c| c.token.clone())
            .unwrap_or_default()
    }

    /// Route this task's abort to the operation's own cancel op.
    pub(crate) fn set_canceller(&self, id: &str, canceller: TaskCanceller) {
        if let Some(c) = self.lock().get_mut(id) {
            c.canceller = Some(canceller);
        }
    }

    /// Whether `task_abort` has been requested for this task.
    pub(crate) fn abort_requested(&self, id: &str) -> bool {
        self.lock()
            .get(id)
            .map(|c| c.abort_requested)
            .unwrap_or(false)
    }

    /// Mark the abort requested and hand back what the abort needs;
    /// `None` when the task has no live control (it finished).
    fn request_abort(
        &self,
        id: &str,
    ) -> Option<(
        CancellationToken,
        Option<TaskCanceller>,
        watch::Receiver<bool>,
    )> {
        let mut map = self.lock();
        let c = map.get_mut(id)?;
        c.abort_requested = true;
        Some((c.token.clone(), c.canceller.clone(), c.finished.subscribe()))
    }

    /// The task finished: wake abort waiters and drop the control.
    fn finished(&self, id: &str) {
        if let Some(c) = self.lock().remove(id) {
            let _ = c.finished.send(true);
        }
    }

    pub(crate) fn grace(&self) -> Duration {
        match self.grace_ms.load(Ordering::Relaxed) {
            0 => DEFAULT_ABORT_GRACE,
            ms => Duration::from_millis(ms),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_grace(&self, grace: Duration) {
        self.grace_ms
            .store(grace.as_millis().max(1) as u64, Ordering::Relaxed);
    }
}

/// Insert `key` into a detail value, wrapping a non-object detail.
fn annotate(detail: &mut Value, key: &str, value: Value) {
    if !detail.is_object() {
        let old = std::mem::take(detail);
        *detail = if old.is_null() {
            json!({})
        } else {
            json!({ "detail": old })
        };
    }
    if let Some(obj) = detail.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
}

/// Apply a finish to a record (see the module docs for the rules).
fn settle(rec: &mut TaskRecord, status: TaskStatus, detail: Value, abort_requested: bool) {
    if rec.status == TaskStatus::Aborted {
        // Already aborted after the grace period: keep the abort and
        // record what the work eventually did.
        annotate(
            &mut rec.detail,
            "late_outcome",
            json!({ "status": status, "detail": detail }),
        );
        return;
    }
    let status = if abort_requested && status != TaskStatus::Done {
        TaskStatus::Aborted
    } else {
        status
    };
    rec.status = status;
    rec.detail = detail;
    if abort_requested {
        annotate(&mut rec.detail, "abort_requested", json!(true));
    }
}

/// Body of `AppState::finish_task`.
pub(crate) async fn finish(state: &AppState, id: &str, status: TaskStatus, detail: Value) {
    {
        let mut tasks = state.tasks.lock().await;
        let abort_requested = state.task_controls.abort_requested(id);
        if let Some(rec) = tasks.get_mut(id) {
            settle(rec, status, detail, abort_requested);
            crate::event_bus::publish_task(state, rec);
        }
    }
    // After the record is settled, so a waiting abort reads the final
    // status.
    state.task_controls.finished(id);
}

/// `task_abort`: cancel the task's work and wait (bounded) for it to
/// stop. See the module docs.
pub(crate) async fn abort(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    task_id: String,
) -> ApiResult {
    let status = current_status(state, &task_id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("task {task_id}")))?;
    if status != TaskStatus::Running {
        return Ok(json!({
            "ok": true,
            "task_id": task_id,
            "status": status,
            "already_finished": true,
        }));
    }
    let Some((token, canceller, mut finished_rx)) = state.task_controls.request_abort(&task_id)
    else {
        // Finished between the status read and here.
        let status = current_status(state, &task_id).await;
        return Ok(json!({
            "ok": true,
            "task_id": task_id,
            "status": status,
            "already_finished": true,
        }));
    };
    token.cancel();
    let routed_to = match &canceller {
        Some(c) => {
            let result = c.invoke(state, sink).await;
            Some(json!({
                "op": c.op_name(),
                "result": match result {
                    Ok(v) => v,
                    Err(e) => json!({ "error": e.to_string() }),
                },
            }))
        }
        None => None,
    };
    // The wait and the forced mark run detached: a caller that drops
    // the request (an HTTP client disconnecting) must not leave the
    // task running forever with its abort half-applied.
    let grace = state.task_controls.grace();
    let enforcer_state = Arc::clone(state);
    let enforcer_id = task_id.clone();
    let enforcer = tokio::spawn(async move {
        let stopped = tokio::time::timeout(grace, async {
            // Err: the sender is gone, which also means finished.
            let _ = finished_rx.wait_for(|done| *done).await;
        })
        .await
        .is_ok();
        if !stopped {
            let mut tasks = enforcer_state.tasks.lock().await;
            if let Some(rec) = tasks.get_mut(&enforcer_id) {
                if rec.status == TaskStatus::Running {
                    rec.status = TaskStatus::Aborted;
                    annotate(&mut rec.detail, "abort_forced", json!(true));
                    annotate(
                        &mut rec.detail,
                        "abort_note",
                        json!(format!(
                            "the work did not stop within the {} ms abort grace period; \
                             it may still be running",
                            grace.as_millis()
                        )),
                    );
                    crate::event_bus::publish_task(&enforcer_state, rec);
                }
            }
        }
        stopped
    });
    let stopped = enforcer.await.unwrap_or(false);
    let status = current_status(state, &task_id).await;
    let mut out = json!({
        "ok": true,
        "task_id": task_id,
        "status": status,
        "stopped": stopped,
        "forced": !stopped,
    });
    if let Some(r) = routed_to {
        out["routed_to"] = r;
    }
    Ok(out)
}

async fn current_status(state: &AppState, id: &str) -> Option<TaskStatus> {
    state.tasks.lock().await.get(id).map(|r| r.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_state(prefix: &str) -> (Arc<AppState>, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-tasks-{prefix}-{}-{n}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = manager_config::AppConfig::default_config();
        cfg.data_dir = dir.clone();
        let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
        (state, dir)
    }

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(crate::NoopSink)
    }

    async fn status_of(state: &AppState, id: &str) -> (TaskStatus, Value) {
        let tasks = state.tasks.lock().await;
        let r = tasks.get(id).expect("task recorded");
        (r.status, r.detail.clone())
    }

    /// Aborting a running download stops the transfer (routed to
    /// cancel_download), removes the partial file, and reaches the
    /// aborted terminal state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_running_download_stops_transfer() {
        // A server that accepts and never answers: the transfer sits
        // in its request phase until cancelled.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let accepted2 = Arc::clone(&accepted);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                accepted2.store(true, Ordering::SeqCst);
                held.push(socket);
            }
        });
        let (state, dir) = test_state("dl");
        {
            let mut cfg = state.config.write().await;
            cfg.models.push(manager_config::ModelConfig {
                id: "abort-probe".to_string(),
                gguf_path: PathBuf::from("models/abort-probe.gguf"),
                quant: "Q4_K_M".to_string(),
                params_b: 1.0,
                sha256: "a".repeat(64),
                verified_at: None,
                verified_sha256: None,
                source_url: format!("https://{addr}/abort-probe.gguf"),
                assigned_backends: Vec::new(),
                enabled: true,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
                mtp: None,
            });
        }
        let sink = sink();
        let started = crate::op_download_model(&state, &sink, "abort-probe".to_string())
            .await
            .unwrap();
        let task_id = started["task_id"].as_str().unwrap().to_string();
        let dest = PathBuf::from(started["dest"].as_str().unwrap());
        for _ in 0..300 {
            if accepted.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(accepted.load(Ordering::SeqCst), "download never connected");
        assert_eq!(status_of(&state, &task_id).await.0, TaskStatus::Running);

        let out = crate::op_task_abort(&state, &sink, task_id.clone())
            .await
            .unwrap();
        assert_eq!(out["stopped"], true, "{out}");
        assert_eq!(out["forced"], false, "{out}");
        assert_eq!(out["status"], "aborted", "{out}");
        assert_eq!(out["routed_to"]["op"], "cancel_download", "{out}");
        assert_eq!(out["routed_to"]["result"]["cancelled"], true, "{out}");
        assert_eq!(status_of(&state, &task_id).await.0, TaskStatus::Aborted);
        assert!(!dest.exists());
        assert!(!manager_models::partial_path(&dest).exists());
        // Nothing left in flight: a cancel now has nothing to stop.
        assert!(
            crate::op_cancel_download(&state, &sink, "abort-probe".to_string())
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An operation with its own cancel mechanism (here the shared
    /// Optimize job slot) is stopped through that mechanism.
    #[tokio::test]
    async fn abort_routes_to_the_operations_own_cancel() {
        let (state, dir) = test_state("route");
        let task_id = state.record_task("optimize-measure", json!({})).await;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *state.optimize_job_abort.lock().await = Some(tx);
        state
            .task_controls
            .set_canceller(&task_id, TaskCanceller::OptimizeJob);
        // The job observes only its slot's abort handle, not the token.
        let worker_state = Arc::clone(&state);
        let worker_id = task_id.clone();
        tokio::spawn(async move {
            let _ = rx.await;
            worker_state
                .finish_task(
                    &worker_id,
                    TaskStatus::Aborted,
                    json!({ "cancelled": true }),
                )
                .await;
        });
        let out = crate::op_task_abort(&state, &sink(), task_id.clone())
            .await
            .unwrap();
        assert_eq!(out["routed_to"]["op"], "optimize_cancel_job", "{out}");
        assert_eq!(out["routed_to"]["result"]["cancelled"], true, "{out}");
        assert_eq!(out["stopped"], true, "{out}");
        assert_eq!(out["status"], "aborted", "{out}");
        assert!(state.optimize_job_abort.lock().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Work that observes the token stops and its finish is recorded
    /// as the abort.
    #[tokio::test]
    async fn abort_cancels_the_token_and_waits_for_the_work() {
        let (state, dir) = test_state("token");
        let task_id = state.record_task("wizard_step", json!({})).await;
        let token = state.task_controls.token(&task_id);
        let worker_state = Arc::clone(&state);
        let worker_id = task_id.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            // Cleanup takes a moment; the abort waits for it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            worker_state
                .finish_task(
                    &worker_id,
                    TaskStatus::Failed,
                    json!({ "error": "stopped" }),
                )
                .await;
        });
        let out = crate::op_task_abort(&state, &sink(), task_id.clone())
            .await
            .unwrap();
        assert_eq!(out["stopped"], true, "{out}");
        assert_eq!(out["status"], "aborted", "{out}");
        assert!(out.get("routed_to").is_none());
        let (status, detail) = status_of(&state, &task_id).await;
        assert_eq!(status, TaskStatus::Aborted);
        assert_eq!(detail["abort_requested"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Aborting a finished task is a no-op that keeps its real status;
    /// an unknown id is NotFound.
    #[tokio::test]
    async fn abort_of_finished_task_is_a_noop() {
        let (state, dir) = test_state("finished");
        let task_id = state.record_task("audit", json!({})).await;
        state
            .finish_task(&task_id, TaskStatus::Done, json!({ "done": 3 }))
            .await;
        let out = crate::op_task_abort(&state, &sink(), task_id.clone())
            .await
            .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["already_finished"], true);
        assert_eq!(out["status"], "done");
        assert_eq!(status_of(&state, &task_id).await.0, TaskStatus::Done);
        assert!(matches!(
            crate::op_task_abort(&state, &sink(), "task-999999".to_string()).await,
            Err(ApiError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Work that ignores cancellation is marked aborted after the
    /// grace period, with a note; its late finish does not overwrite
    /// the abort.
    #[tokio::test]
    async fn task_ignoring_cancel_is_aborted_after_grace() {
        let (state, dir) = test_state("grace");
        state.task_controls.set_grace(Duration::from_millis(100));
        let task_id = state
            .record_task("backend_start", json!({ "backend_id": "b" }))
            .await;
        let started = std::time::Instant::now();
        let out = crate::op_task_abort(&state, &sink(), task_id.clone())
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(out["forced"], true, "{out}");
        assert_eq!(out["stopped"], false, "{out}");
        assert_eq!(out["status"], "aborted", "{out}");
        let (status, detail) = status_of(&state, &task_id).await;
        assert_eq!(status, TaskStatus::Aborted);
        assert!(detail["abort_note"]
            .as_str()
            .unwrap()
            .contains("may still be running"));
        assert_eq!(detail["backend_id"], "b");
        // The work eventually finishes: the abort stands, the outcome
        // is kept alongside.
        state
            .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": "b" }))
            .await;
        let (status, detail) = status_of(&state, &task_id).await;
        assert_eq!(status, TaskStatus::Aborted);
        assert_eq!(detail["late_outcome"]["status"], "done");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn record(status: TaskStatus) -> TaskRecord {
        TaskRecord {
            id: "task-1".into(),
            kind: "k".into(),
            status,
            detail: json!({ "a": 1 }),
            created_ms: 0,
        }
    }

    #[test]
    fn settle_rules() {
        // Plain finish.
        let mut r = record(TaskStatus::Running);
        settle(&mut r, TaskStatus::Failed, json!({ "e": 1 }), false);
        assert_eq!(r.status, TaskStatus::Failed);
        // A non-success finish after an abort request is an abort.
        let mut r = record(TaskStatus::Running);
        settle(&mut r, TaskStatus::Failed, json!({ "e": 1 }), true);
        assert_eq!(r.status, TaskStatus::Aborted);
        assert_eq!(r.detail["abort_requested"], true);
        // Work that completed before the cancel took hold stays done.
        let mut r = record(TaskStatus::Running);
        settle(&mut r, TaskStatus::Done, json!({}), true);
        assert_eq!(r.status, TaskStatus::Done);
        // A forced abort is not overwritten by the late finish.
        let mut r = record(TaskStatus::Aborted);
        settle(&mut r, TaskStatus::Done, json!({ "x": 2 }), true);
        assert_eq!(r.status, TaskStatus::Aborted);
        assert_eq!(r.detail["late_outcome"]["status"], "done");
        assert_eq!(r.detail["a"], 1);
    }

    #[test]
    fn annotate_wraps_non_objects() {
        let mut v = json!("text");
        annotate(&mut v, "k", json!(1));
        assert_eq!(v, json!({ "detail": "text", "k": 1 }));
        let mut v = Value::Null;
        annotate(&mut v, "k", json!(1));
        assert_eq!(v, json!({ "k": 1 }));
    }
}
