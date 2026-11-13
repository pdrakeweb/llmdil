//! Live state for Optimize Phase 3 (docs/optimize-design.md §7):
//! the streaming consultation's progress record and the Rust apply
//! executor's one-at-a-time flag.
//!
//! The consultation's progress is written from the streaming
//! client's per-chunk callback (a plain `std::sync::Mutex`, held
//! for a field copy, never across an await) and read by
//! `state_payload` as the additive `consult_progress` field, so
//! every page instance — not only the one that started the
//! consultation — can show tokens-so-far and partial-answer size
//! beside the elapsed time it already shows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

/// One running consultation's progress.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ConsultProgress {
    pub backend_id: String,
    /// Text chunks received (≈ tokens; llama-server emits one per
    /// sampled token).
    pub chunks: u64,
    pub content_bytes: usize,
    pub reasoning_bytes: usize,
    /// Time from request send to the latest chunk.
    pub elapsed_ms: u64,
    pub first_token_ms: Option<u64>,
}

/// Phase 3 live state, one per `AppState`.
#[derive(Default)]
pub(crate) struct OptimizeLive {
    progress: Mutex<Option<ConsultProgress>>,
    applying: AtomicBool,
}

impl OptimizeLive {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<ConsultProgress>> {
        self.progress.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A consultation started against `backend_id`: progress is
    /// zero until the first chunk arrives.
    pub(crate) fn begin_consult(&self, backend_id: &str) {
        *self.slot() = Some(ConsultProgress {
            backend_id: backend_id.to_string(),
            ..ConsultProgress::default()
        });
    }

    /// Record the streaming client's latest progress report.
    pub(crate) fn record(&self, p: &manager_supervisor::StreamProgress) {
        if let Some(cur) = self.slot().as_mut() {
            cur.chunks = p.chunks;
            cur.content_bytes = p.content_bytes;
            cur.reasoning_bytes = p.reasoning_bytes;
            cur.elapsed_ms = p.elapsed_ms;
            cur.first_token_ms = p.first_token_ms;
        }
    }

    /// The consultation ended (any way).
    pub(crate) fn end_consult(&self) {
        *self.slot() = None;
    }

    pub(crate) fn progress(&self) -> Option<ConsultProgress> {
        self.slot().clone()
    }

    /// The payload's `consult_progress` value: the record, or null
    /// when no consultation is streaming.
    pub(crate) fn progress_json(&self) -> Value {
        match self.progress() {
            Some(p) => json!({
                "backend_id": p.backend_id,
                "chunks": p.chunks,
                "content_bytes": p.content_bytes,
                "reasoning_bytes": p.reasoning_bytes,
                "elapsed_ms": p.elapsed_ms,
                "first_token_ms": p.first_token_ms,
                // Seen at least one chunk: the page can say
                // "generating" instead of "waiting for the first
                // token" (prompt processing).
                "streaming": p.chunks > 0,
            }),
            None => Value::Null,
        }
    }

    /// Claim the apply executor; false when an apply is running.
    pub(crate) fn try_begin_apply(&self) -> bool {
        self.applying
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn end_apply(&self) {
        self.applying.store(false, Ordering::Release);
    }

    pub(crate) fn applying(&self) -> bool {
        self.applying.load(Ordering::Acquire)
    }
}

/// Clears the consultation's progress record on every exit path
/// (success, failure, cancellation, and a dropped future).
pub(crate) struct ConsultProgressGuard<'a>(pub(crate) &'a OptimizeLive);

impl Drop for ConsultProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.end_consult();
    }
}

/// Releases the apply executor claim on every exit path.
pub(crate) struct ApplyGuard<'a>(pub(crate) &'a OptimizeLive);

impl Drop for ApplyGuard<'_> {
    fn drop(&mut self) {
        self.0.end_apply();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lifecycle_and_json() {
        let live = OptimizeLive::default();
        assert_eq!(live.progress_json(), Value::Null);
        {
            let _g = ConsultProgressGuard(&live);
            live.begin_consult("coder");
            let v = live.progress_json();
            assert_eq!(v["backend_id"], json!("coder"));
            assert_eq!(v["chunks"], json!(0));
            assert_eq!(v["streaming"], json!(false));
            live.record(&manager_supervisor::StreamProgress {
                chunks: 7,
                content_bytes: 40,
                reasoning_bytes: 0,
                elapsed_ms: 1200,
                first_token_ms: Some(900),
            });
            let v = live.progress_json();
            assert_eq!(v["chunks"], json!(7));
            assert_eq!(v["content_bytes"], json!(40));
            assert_eq!(v["first_token_ms"], json!(900));
            assert_eq!(v["streaming"], json!(true));
        }
        assert_eq!(live.progress_json(), Value::Null, "guard clears on drop");
        // A report with no consultation running is ignored.
        live.record(&manager_supervisor::StreamProgress::default());
        assert!(live.progress().is_none());
    }

    #[test]
    fn apply_claim_is_exclusive() {
        let live = OptimizeLive::default();
        assert!(live.try_begin_apply());
        assert!(!live.try_begin_apply());
        {
            let _g = ApplyGuard(&live);
        }
        assert!(!live.applying());
        assert!(live.try_begin_apply());
    }
}
