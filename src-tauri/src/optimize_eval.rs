//! Optimize screening-eval commands (design addendum A.4,
//! tracker #28 Phase 5d): the suite runner and the standalone
//! baseline command.
//!
//! The suite definition, checkers, and scoring live in
//! `manager_optimize::eval` (pure, unit-tested); this module is
//! the I/O glue: it runs the tasks sequentially against a
//! running backend over loopback via
//! `manager_supervisor::chat_completion` at temperature 0 with
//! per-task token caps, inside the shared Optimize job slot
//! (the same slot measurements use, with cancellation by future
//! drop), inside the §A.4 ten-minute per-backend time-box.
//!
//! The baseline protocol: `optimize_run_baseline` runs the
//! suite against the incumbent backend and stores the result
//! in `<data_dir>/optimize-eval.json`, keyed by the backend's
//! current measurement fingerprint (§A.1) — the same fingerprint
//! the measurement store uses. When the backend's configuration
//! changes, its fingerprint changes, and the stored baseline
//! surfaces as stale rather than being presented as current;
//! it is never silently re-keyed. Experiment runs (Phase 5e)
//! use [`run_eval_suite`] directly and compare against the
//! stored baseline with `manager_optimize::compare`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;

use manager_optimize::{
    eval_suite, failed_task, score_task, BaselineRecord, EvalRunRecord, EvalTaskResult,
    EVAL_SUITE_VERSION, EVAL_TEMPERATURE, EVAL_TIME_BOX_SECS, SCREENING_CAVEAT,
};

use crate::{err, ApiError, ApiResult, AppState, TaskStatus};

// ---------------------------------------------------------------------------
// Baseline persistence (<data_dir>/optimize-eval.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct EvalFile {
    #[serde(default)]
    baseline: Option<BaselineRecord>,
}

fn eval_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-eval.json")
}

fn load_eval_file(state: &AppState) -> EvalFile {
    std::fs::read_to_string(eval_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_eval_file(state: &AppState, file: &EvalFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(eval_path(state), text).map_err(|e| e.to_string())
}

/// The `eval_baseline` payload field (consumed by the Optimize
/// page and, for delta rendering, by Phase 5e): the stored
/// baseline's headline scores beside its provenance, or `null`
/// when no baseline has been run. `stale` is true when the
/// stored fingerprint differs from the backend's current
/// measurement fingerprint, when the backend no longer exists,
/// or when the baseline was recorded by a different suite
/// version. The verbatim §A.4 caveat rides with the scores —
/// it is stated wherever scores appear.
pub(crate) fn eval_baseline_json(
    state: &AppState,
    current_fingerprints: &std::collections::HashMap<String, String>,
) -> Value {
    let file = load_eval_file(state);
    let Some(baseline) = file.baseline else {
        return Value::Null;
    };
    let stale = baseline.suite_version != EVAL_SUITE_VERSION
        || current_fingerprints.get(&baseline.backend_id) != Some(&baseline.config_fingerprint);
    json!({
        "backend_id": baseline.backend_id,
        "config_fingerprint": baseline.config_fingerprint,
        "suite_version": baseline.suite_version,
        "run_at": baseline.run_at,
        "overall": baseline.scores.overall,
        "per_category": baseline.scores.per_category,
        "stale": stale,
        "caveat": SCREENING_CAVEAT,
    })
}

// ---------------------------------------------------------------------------
// The suite runner
// ---------------------------------------------------------------------------

/// Run the whole screening suite against one backend,
/// sequentially: one `chat_completion` per task at temperature
/// 0 with the task's token cap, a single system instruction
/// shared by every task so the runs are identical apart from
/// the model under test.
///
/// The §A.4 time-box is enforced here: tasks that have not
/// started when the box expires are recorded as failed with
/// that reason, and a task whose request overruns the box is
/// cut off the same way (its per-task timeout is the remaining
/// box). Transport failures likewise fail just that task, with
/// the error as the reason — one broken task never discards
/// the rest of the run's evidence. Cancellation is by future
/// drop: the caller selects this future against the job slot's
/// abort receiver, exactly as the measurement runner does, so
/// dropping it aborts whichever request is in flight; nothing
/// is persisted by this function.
pub async fn run_eval_suite(backend_id: &str, base_url: &str, model: &str) -> EvalRunRecord {
    let suite = eval_suite();
    let run_at = chrono::Utc::now().to_rfc3339();
    let deadline = Instant::now() + Duration::from_secs(EVAL_TIME_BOX_SECS);
    let mut results: Vec<EvalTaskResult> = Vec::with_capacity(suite.len());
    for task in &suite {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            results.push(failed_task(task, "time-box expired before this task ran"));
            continue;
        }
        let messages = vec![
            manager_supervisor::ChatMessage {
                role: "system".to_string(),
                content: "You are completing a screening evaluation. Follow each task's instructions exactly and output only what the task asks for.".to_string(),
            },
            manager_supervisor::ChatMessage {
                role: "user".to_string(),
                content: task.prompt.clone(),
            },
        ];
        match manager_supervisor::chat_completion(
            base_url,
            model,
            &messages,
            EVAL_TEMPERATURE,
            task.max_tokens,
            remaining,
        )
        .await
        {
            Ok(result) => results.push(score_task(task, &result.content)),
            Err(e) => {
                let reason = if Instant::now() >= deadline {
                    "time-box expired during this task".to_string()
                } else {
                    format!("request failed: {e}")
                };
                results.push(failed_task(task, reason));
            }
        }
    }
    EvalRunRecord::new(backend_id.to_string(), run_at, results)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Run the screening suite against a running backend and store
/// the result as the baseline (Phase 5d, §A.4): the standalone
/// command for the protocol the experiment batch (5e) also
/// uses. The backend must be running — the page never starts
/// one (§3's rule, shared with consultations and measurements).
/// The run occupies the shared GPU-job slot as tracked task
/// kind `optimize-eval-baseline`, so it is cancellable through
/// `optimize_cancel_job` and never overlaps a measurement or an
/// experiment batch.
#[tauri::command]
pub async fn optimize_run_baseline(state: State<'_, AppState>, backend_id: String) -> ApiResult {
    {
        let slot = state.optimize_job_abort.lock().await;
        if slot.is_some() {
            return Err(err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            )));
        }
    }
    let cfg = state.config.read().await.clone();
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == backend_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {backend_id}"))))?
        .clone();
    // The baseline is keyed by the backend's *current*
    // measurement fingerprint, computed from the same live
    // snapshot the state payload uses.
    let gathered = crate::optimize::gather(&state, &cfg).await;
    if gathered.running.get(&backend_id).copied() != Some(true) {
        return Err(err(ApiError::InvalidInput(format!(
            "backend '{backend_id}' is not running; the screening suite runs against a running backend"
        ))));
    }
    let fingerprint = gathered
        .fingerprints
        .get(&backend_id)
        .cloned()
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {backend_id}"))))?;
    let model_id = crate::model_for_backend(&cfg, &backend)
        .map(|m| m.id.clone())
        .unwrap_or_else(|| {
            backend
                .model_file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| backend.id.clone())
        });
    let task_id = state
        .record_task(
            "optimize-eval-baseline",
            json!({
                "backend": backend_id,
                "fingerprint": fingerprint,
                "suite_version": EVAL_SUITE_VERSION,
            }),
        )
        .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.optimize_job_abort.lock().await;
        if slot.is_some() {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": "a measurement or experiment job is already running" }),
                )
                .await;
            return Err(err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            )));
        }
        *slot = Some(tx);
    }
    // Loopback by construction, as with consultations (§3).
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let run = run_eval_suite(&backend_id, &base_url, &model_id);
    let outcome = tokio::select! {
        res = run => Some(res),
        _ = rx => None,
    };
    {
        let mut slot = state.optimize_job_abort.lock().await;
        *slot = None;
    }
    let record = match outcome {
        None => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Aborted,
                    json!({ "backend": backend_id }),
                )
                .await;
            return Err("screening eval cancelled".to_string());
        }
        Some(record) => record,
    };
    let baseline = BaselineRecord::from_run(&record, fingerprint.clone());
    save_eval_file(
        &state,
        &EvalFile {
            baseline: Some(baseline),
        },
    )?;
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({
                "backend": backend_id,
                "fingerprint": fingerprint,
                "passed": record.scores.overall.passed,
                "total": record.scores.overall.total,
            }),
        )
        .await;
    crate::optimize::state_payload(&state).await
}
