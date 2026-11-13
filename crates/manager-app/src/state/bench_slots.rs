//! Bench subsystem state: the sweep's cancel slot and descriptor,
//! and the prompt-generation cancel slot.
//!
//! Each slot remains its own lock (none merged, none changed kind).
//! The sweep start checks `job` and then the Optimize
//! `job_abort` slot, each in its own short critical section.

use tokio::sync::Mutex;

use crate::bench;

/// The Bench page's job slots.
#[derive(Default)]
pub(crate) struct BenchState {
    /// Bench sweep cancellation slot (Bench design §6.4): the
    /// running sweep's scoped cancel handle, owned by the sweep
    /// rather than the task bookkeeping (D13) — the same shape
    /// as the Optimize job slot, a separate slot because a sweep
    /// and an Optimize job are different jobs with different
    /// lifetimes.
    pub(crate) abort: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Prompt-generation cancellation slot (Bench design §4.3):
    /// the stored abort handle for the in-flight draft
    /// generation, mirroring the consultation's slot.
    pub(crate) generate_abort: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// The running Bench sweep's descriptor (id + progress),
    /// read by `bench_results_list` so any page instance can
    /// render the sweep's progress.
    pub(crate) job: Mutex<Option<bench::BenchJob>>,
}
