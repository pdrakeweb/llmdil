//! Optimize subsystem state: the consultation slot (abort handle,
//! start time, last failure, streaming progress) and the GPU-job
//! slot (abort handle, running job, last failure), plus the
//! per-backend record of running operations that serializes the two
//! families against one backend (tracker #70).
//!
//! Every slot remains its own lock, exactly as it was as a separate
//! `AppState` field (none was merged, none changed kind). The Bench
//! sweep also reads `job_abort` to refuse a sweep while a
//! measurement/experiment job runs (see [`super::BenchState`]).

use std::collections::HashMap;

use tokio::sync::Mutex;

use crate::optimize_stream;
use crate::state::{OptimizeJobInfo, OptimizeRunningOp};

/// The Optimize page's job slots.
#[derive(Default)]
pub(crate) struct OptimizeState {
    /// Shared cancellation slot for the Optimize consultation (the
    /// design's one-consult-at-a-time rule): starting a consult
    /// stores its sender; firing it aborts the in-flight LLM call.
    pub(crate) consult_abort: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// When the running consultation started (Unix ms), recorded
    /// beside the abort slot so the state payload can report a
    /// true elapsed time to any page instance — the page's own
    /// clock only knows about consultations it started itself.
    pub(crate) consult_started_unix_ms: Mutex<Option<i64>>,
    /// The last consultation's failure (message + Unix ms),
    /// recorded when a run ends in failure and cleared when a
    /// new run starts or one succeeds: a consultation that ran
    /// while the operator was on another view must still surface
    /// its failure on the Optimize page, not silently leave the
    /// previous plan looking current.
    pub(crate) consult_error: Mutex<Option<(String, i64)>>,
    /// Optimize Phase 3 live state: streaming-consultation progress
    /// and the Rust apply executor's one-at-a-time claim.
    pub(crate) live: optimize_stream::OptimizeLive,
    /// Shared cancellation slot for GPU-bound Optimize jobs
    /// (Phase 5: measurements and experiment batches): one such job
    /// at a time, separate from the consult slot because a
    /// consultation and a measurement are different jobs with
    /// different lifetimes.
    pub(crate) job_abort: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// The running Estimate/Measure job's descriptor (tracker
    /// #69): kind, backend, and start time, recorded beside the
    /// job slot so the state payload can tell ANY page instance
    /// what is actually running — the payload's shared
    /// `measuring` boolean alone cannot distinguish an Estimate
    /// from a Measure, which is how an Estimate run rendered
    /// with the Measure profile's copy (Oct 20, 2026).
    pub(crate) job: Mutex<Option<OptimizeJobInfo>>,
    /// The last Estimate/Measure job's failure (message + Unix
    /// ms), following the consultation's `consult_error`
    /// pattern: recorded when a job ends in failure, cleared
    /// when a new job starts or one succeeds, so a failure that
    /// lands while the operator is on another view (or in
    /// another window) still surfaces instead of leaving stale
    /// in-progress copy forever.
    pub(crate) job_error: Mutex<Option<(String, i64)>>,
    /// Per-backend record of the currently running Optimize
    /// operation (tracker #70): backend id -> the operation
    /// holding that backend, in one of the two families
    /// (consultation vs benchmark). Consulted at job start on
    /// both paths so a consultation and a benchmark (Measure /
    /// Estimate / eval baseline / an experiment batch's run
    /// against its consulted backend) never run concurrently
    /// against the same backend and store figures depressed by
    /// contention as if they were uncontended. Entries are
    /// claimed at job start and released on every terminal path
    /// (completion, failure, timeout, operator cancel) through
    /// the RAII guard in `optimize.rs`; the cancel commands also
    /// clear the family they cancel. std Mutex: claims, releases,
    /// and the guard's Drop are all synchronous.
    pub(crate) running_ops: std::sync::Mutex<HashMap<String, OptimizeRunningOp>>,
}
