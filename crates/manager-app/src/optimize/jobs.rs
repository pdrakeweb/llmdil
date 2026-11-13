//! Optimize persistence and bookkeeping: the plan file, consult and job
//! records, and the per-family running-operation claims.

use super::*;

// ---------------------------------------------------------------------------
// Plan persistence (<data_dir>/optimize-plan.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct PlanFile {
    #[serde(default)]
    pub(crate) plan: Option<Plan>,
    /// The stored plan came from sample mode, not a consultation.
    #[serde(default)]
    pub(crate) sample: bool,
    /// Denial memory (design §6): fingerprints the operator has
    /// denied, accumulated across plans. Kept when a plan is
    /// discarded — the denials were explicit decisions.
    #[serde(default)]
    pub(crate) denied_fingerprints: Vec<String>,
}

pub(super) fn plan_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-plan.json")
}

pub(crate) fn load_plan_file(state: &AppState) -> PlanFile {
    std::fs::read_to_string(plan_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_plan_file(state: &AppState, file: &PlanFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(plan_path(state), text).map_err(|e| e.to_string())
}

/// Load the stored plan, re-grounding experiment provisioning
/// first (see `manager_optimize::reground_experiment_provisioning`):
/// a plan file written before provisioning was grounded at build
/// time stores no provisioning links, and without this pass every
/// consumer — payload validation, decision cascades, the runner —
/// dead-ends its experiments as unresolvable. A repaired plan is
/// persisted so the fix is durable, not recomputed forever.
pub(crate) async fn load_plan_file_current(state: &AppState) -> PlanFile {
    let mut file = load_plan_file(state);
    let mut repaired = false;
    if let Some(plan) = file.plan.as_mut() {
        let cfg = state.config.read().await.clone();
        repaired = manager_optimize::reground_experiment_provisioning(plan, &cfg);
    }
    if repaired {
        let _ = save_plan_file(state, &file);
    }
    file
}

// ---------------------------------------------------------------------------
// Consultation run record (state payload): when the running
// consultation started, and how the last one failed. Recorded
// server-side because the page instance that started a
// consultation is not necessarily the one looking at the page
// when it ends (the operator may be on the Logs view reading
// the Optimize log — exactly how the Oct 19, 2026 failures
// presented: an epoch-valued elapsed timer and a silently
// retained previous plan).
// ---------------------------------------------------------------------------

pub(super) async fn record_consult_start(state: &AppState) {
    *state.optimize.consult_started_unix_ms.lock().await =
        Some(chrono::Utc::now().timestamp_millis());
    *state.optimize.consult_error.lock().await = None;
}

pub(super) async fn record_consult_failure(state: &AppState, message: String) {
    *state.optimize.consult_started_unix_ms.lock().await = None;
    *state.optimize.consult_error.lock().await =
        Some((message, chrono::Utc::now().timestamp_millis()));
}

pub(super) async fn record_consult_success(state: &AppState) {
    *state.optimize.consult_started_unix_ms.lock().await = None;
    *state.optimize.consult_error.lock().await = None;
}

// ---------------------------------------------------------------------------
// Estimate/Measure job record (tracker #69): the running job's
// descriptor and the last job's failure, recorded server-side
// for the same reason the consultation record above exists —
// the page instance that started a job is not necessarily the
// one looking at the page when it ends. Before this record the
// payload carried only the shared `measuring` boolean, so a
// second window could not tell an Estimate from a Measure (it
// rendered the Measure profile's copy during Estimate runs),
// had no start time for an elapsed figure, and never learned
// that a job had failed.
// ---------------------------------------------------------------------------

/// Record a benchmark job's start: its descriptor replaces any
/// previous one and clears the previous job's recorded failure
/// (the `consult_error` discipline: a new run supersedes the
/// last failure).
pub(crate) async fn record_job_start(state: &AppState, kind: &str, backend_id: &str) -> i64 {
    // The start time doubles as the job's identity token for
    // its terminal records (see `job_is_current`), so it must
    // be unique per job, not just per millisecond: a cancelled
    // job and its replacement can start within the same
    // millisecond. Keep it strictly increasing (skew vs the
    // wall clock is at most a few milliseconds, immaterial for
    // the elapsed display it also feeds).
    static LAST_JOB_START_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let started_unix_ms = loop {
        let prev = LAST_JOB_START_MS.load(std::sync::atomic::Ordering::Relaxed);
        let next = chrono::Utc::now().timestamp_millis().max(prev + 1);
        if LAST_JOB_START_MS
            .compare_exchange(
                prev,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            break next;
        }
    };
    *state.optimize.job.lock().await = Some(crate::OptimizeJobInfo {
        kind: kind.to_string(),
        backend_id: backend_id.to_string(),
        started_unix_ms,
    });
    *state.optimize.job_error.lock().await = None;
    started_unix_ms
}

/// Whether the recorded descriptor is still the job that
/// started at `started_unix_ms`: a job's terminal record only
/// applies while its own descriptor is current. Since tracker
/// #70 a cancel frees the backend at once, so a cancelled job's
/// late unwind can race a newer job's start — the newer job's
/// descriptor (and its cleared error state) must survive the
/// older job's terminal record, the same token discipline the
/// running-operation record's claim ids provide there.
pub(super) async fn job_is_current(state: &AppState, started_unix_ms: i64) -> bool {
    state
        .optimize
        .job
        .lock()
        .await
        .as_ref()
        .map(|j| j.started_unix_ms == started_unix_ms)
        .unwrap_or(false)
}

/// Record a benchmark job's failure: the descriptor clears (the
/// job is over) and the failure is kept for the payload's
/// `job_error`, so every window surfaces it instead of stale
/// in-progress copy.
pub(crate) async fn record_job_failure(state: &AppState, started_unix_ms: i64, message: String) {
    if !job_is_current(state, started_unix_ms).await {
        return;
    }
    *state.optimize.job.lock().await = None;
    *state.optimize.job_error.lock().await = Some((message, chrono::Utc::now().timestamp_millis()));
}

/// Record a benchmark job's success: descriptor and recorded
/// failure both clear.
pub(crate) async fn record_job_success(state: &AppState, started_unix_ms: i64) {
    if !job_is_current(state, started_unix_ms).await {
        return;
    }
    *state.optimize.job.lock().await = None;
    *state.optimize.job_error.lock().await = None;
}

/// Record a benchmark job's end without a verdict (operator
/// cancellation): the descriptor clears; no failure is recorded
/// — a deliberate cancel is not a failure, and the cancelling
/// page reports it through its own return value.
pub(crate) async fn record_job_end(state: &AppState, started_unix_ms: i64) {
    if !job_is_current(state, started_unix_ms).await {
        return;
    }
    *state.optimize.job.lock().await = None;
}

/// The `job` payload value for a descriptor: the descriptor
/// object, or `null` when no Estimate/Measure job is running.
pub(crate) fn job_json(job: &Option<crate::OptimizeJobInfo>) -> Value {
    match job {
        Some(j) => json!({
            "kind": j.kind,
            "backend_id": j.backend_id,
            "started_unix_ms": j.started_unix_ms,
        }),
        None => Value::Null,
    }
}

// Consultation/benchmark mutual exclusion per backend
// (tracker #70): a single per-backend record of the currently
// running Optimize operation family, consulted at job start on
// both paths (consultation and benchmark). A benchmark refused
// while a consultation runs on the same backend — and the
// reverse — instead of silently storing measurements depressed
// by contention as if they were uncontended (the Oct 20, 2026
// Estimate-during-consultation case). Refusal only: no queueing,
// no preemption, no automatic retry. Different backends are
// unaffected. The record is held through an RAII guard so every
// terminal path — completion, failure, timeout (a dropped job
// future drops the guard), and operator cancel — clears it; the
// cancel commands additionally clear the family they cancel, so
// a start is allowed immediately after a cancel even before the
// cancelled operation's future unwinds.
// ---------------------------------------------------------------------------

/// The noun phrase naming one operation kind, used on both sides
/// of a refusal ("cannot start a measurement ... : a consultation
/// is already running ...").
pub(super) fn op_noun(kind: &str) -> String {
    match kind {
        "consultation" => "a consultation".to_string(),
        "measure" => "a measurement".to_string(),
        "estimate" => "an estimate".to_string(),
        "eval-baseline" => "a screening-eval baseline run".to_string(),
        "experiment" => "an experiment run".to_string(),
        "experiment-estimate" => "an experiment estimate run".to_string(),
        other => format!("a {other} operation"),
    }
}

/// How long an operation has been running, in the refusal's
/// "started ... ago" form: seconds under a minute, then
/// minutes + seconds.
pub(super) fn op_elapsed_text(started_unix_ms: i64) -> String {
    let elapsed_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(started_unix_ms)
        .max(0);
    let secs = elapsed_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// The refusal for a start that would contend with the running
/// operation recorded for the same backend: it names the running
/// operation (kind, and its start elapsed — the record carries
/// the start the tracker #69 job-state payload also exposes).
pub(super) fn contention_refusal(existing: &OptimizeRunningOp, new_kind: &str) -> ApiError {
    ApiError::InvalidInput(format!(
        "cannot start {} on backend '{}': {} is already running on that backend (started {} ago); consultations and benchmarks on the same backend are mutually exclusive so benchmark figures are not taken under contention — wait for the running operation to finish or cancel it first",
        op_noun(new_kind),
        existing.backend_id,
        op_noun(&existing.kind),
        op_elapsed_text(existing.started_unix_ms),
    ))
}

/// Claim the per-backend running-operation record for a new
/// operation. An existing record for the SAME backend refuses
/// the start, whichever family holds it (within-family starts
/// are already serialized by the consult / GPU-job slots, so a
/// same-family record here is a second start all the same);
/// a record for a different backend never refuses.
pub(crate) fn try_claim_optimize_op(
    state: &AppState,
    backend_id: &str,
    family: OptimizeOpFamily,
    kind: &str,
) -> Result<OptimizeRunningOp, ApiError> {
    let mut ops = state
        .optimize
        .running_ops
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = ops.get(backend_id) {
        let err = contention_refusal(existing, kind);
        if let ApiError::InvalidInput(msg) = &err {
            opt_log1(state, &format!("mutual exclusion refusal: {msg}"));
        }
        return Err(err);
    }
    static NEXT_CLAIM_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let record = OptimizeRunningOp {
        family,
        kind: kind.to_string(),
        backend_id: backend_id.to_string(),
        started_unix_ms: chrono::Utc::now().timestamp_millis(),
        claim_id: NEXT_CLAIM_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    };
    ops.insert(backend_id.to_string(), record.clone());
    Ok(record)
}

/// Release the record carrying the given claim token, if it is
/// still that claim's record: a release arriving after a cancel
/// already cleared the record (and a newer operation claimed
/// the backend) must not clear the newer record.
pub(crate) fn release_optimize_op(state: &AppState, backend_id: &str, claim_id: u64) {
    let mut ops = state
        .optimize
        .running_ops
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if ops
        .get(backend_id)
        .map(|op| op.claim_id == claim_id)
        .unwrap_or(false)
    {
        ops.remove(backend_id);
    }
}

/// Clear every running-operation record of one family (the
/// cancel commands: the cancelled operation's own guard release
/// then finds nothing — or a newer claim — and leaves it alone).
pub(crate) fn clear_running_ops(state: &AppState, family: OptimizeOpFamily) {
    let mut ops = state
        .optimize
        .running_ops
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    ops.retain(|_, op| op.family != family);
}

/// The RAII hold on one claimed record: dropping it releases the
/// record, so no exit path of a claimed operation — success,
/// failure, an early `?`, a timeout or cancellation that drops
/// the operation's future — can leave a stale record behind.
pub(crate) struct OptimizeOpGuard {
    pub(super) state: std::sync::Arc<AppState>,
    pub(super) backend_id: String,
    pub(super) claim_id: u64,
}

impl OptimizeOpGuard {
    pub(crate) fn claim(
        state: &std::sync::Arc<AppState>,
        backend_id: &str,
        family: OptimizeOpFamily,
        kind: &str,
    ) -> Result<Self, ApiError> {
        let record = try_claim_optimize_op(state, backend_id, family, kind)?;
        Ok(Self {
            state: state.clone(),
            backend_id: record.backend_id,
            claim_id: record.claim_id,
        })
    }
}

impl Drop for OptimizeOpGuard {
    fn drop(&mut self) {
        release_optimize_op(&self.state, &self.backend_id, self.claim_id);
    }
}

/// The `running_ops` payload value: every backend's running
/// Optimize operation (family, kind, start), or an empty object
/// when none is running — the same naming the refusals use, for
/// any page instance.
pub(super) fn running_ops_json(state: &AppState) -> Value {
    let ops = state
        .optimize
        .running_ops
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut map = serde_json::Map::new();
    for (backend_id, op) in ops.iter() {
        map.insert(
            backend_id.clone(),
            json!({
                "family": match op.family {
                    OptimizeOpFamily::Consultation => "consultation",
                    OptimizeOpFamily::Benchmark => "benchmark",
                },
                "kind": op.kind,
                "backend_id": op.backend_id,
                "started_unix_ms": op.started_unix_ms,
            }),
        );
    }
    Value::Object(map)
}

pub(crate) fn measurements_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-measurements.json")
}
