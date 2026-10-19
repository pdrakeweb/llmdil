//! Activation plan execution history: a durable record of every
//! activation plan apply — what the plan contained, what was done
//! about each item, and how the execution ended.
//!
//! An apply is orchestrated by the desktop UI (see
//! `applyActivationPlan` in the UI layer: restart the named
//! backends, then the gateway side through the start-gateway step).
//! Before this module, the outcome existed only as transient status
//! strings on the surface that ran it — dismiss the view and the
//! record of a half-failed apply (the case an operator most needs
//! to review) was gone. The per-change activation journal in
//! [`crate::activation`] does not fill the gap: it records saved
//! CONFIG changes for the pending-plan machinery and resolves them
//! as processes (re)start; it never knew what an apply attempted or
//! how each item ended.
//!
//! Discipline, mirroring the Optimize log's best-effort appends
//! (tracker #52): recording must never block, fail, or alter an
//! activation. [`record_execution`] swallows every failure and
//! reports `false`; the history reader degrades a missing or
//! corrupt file to an empty list rather than an error, so the
//! Activation Plan page renders the same either way.
//!
//! Retention is capped: [`HISTORY_CAP`] executions. Each record
//! carries the full plan item list (operator-facing summaries, tens
//! of items at most), so 50 executions — months of applies at the
//! cadence this app sees — keep the file well under a megabyte
//! while covering any realistic "what did that apply do" question.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Most executions retained; the oldest are dropped first.
pub const HISTORY_CAP: usize = 50;

/// One plan change as the plan presented it at apply time, with the
/// outcome the apply recorded for it. The descriptive fields mirror
/// the `gateway_activation_plan` payload's change entries exactly —
/// this is the plan AS COMPUTED WHEN THE APPLY RAN, which a later
/// plan recomputation cannot reproduce.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivationExecutionItem {
    /// RFC 3339 timestamp of the save that journaled the change.
    #[serde(default)]
    pub ts: String,
    /// Journal kind of the change (`backend_flags`, `gateway_config`, …).
    #[serde(default)]
    pub kind: String,
    /// Operator-facing one-line summary, as rendered in the plan.
    #[serde(default)]
    pub summary: String,
    /// `gateway` or `backend` (the change's activation scope).
    #[serde(default)]
    pub scope: String,
    /// Backend ids a backend-scope change applies to.
    #[serde(default)]
    pub targets: Vec<String>,
    /// How the change was classified to go live at apply time:
    /// `restart`, `gateway_restart`, or `next_start`.
    #[serde(default)]
    pub activation: String,
    /// `applied`, `skipped`, or `failed`.
    #[serde(default)]
    pub outcome: String,
    /// Why a skipped item was skipped, or a failed item's error
    /// text. `None` for applied items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One activation plan apply, whole. Status vocabulary:
/// `completed` (the apply ran to its end, nothing failed),
/// `completed_with_failures` (it ran to its end with at least one
/// failed item — the gateway step is the last step, so its failure
/// lands here), and `failed` (the apply aborted before every step
/// was attempted, or errored out midway; the per-item outcomes show
/// how far it got).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivationExecution {
    /// Server-assigned when the execution is recorded; a reported
    /// draft's value is ignored.
    #[serde(default)]
    pub id: String,
    /// RFC 3339 timestamps reported by the applying surface (the
    /// desktop UI and the server share a host clock).
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: String,
    /// `completed`, `completed_with_failures`, or `failed`.
    #[serde(default)]
    pub status: String,
    /// The surface that initiated the apply (`desktop` — the
    /// desktop UI funnel is the one apply path that exists; the
    /// REST apply composition in rest-api.md §6.6 is not yet
    /// server-owned). Recorded as reported, never invented.
    #[serde(default)]
    pub source: String,
    /// The apply mode the operator chose: `quiesce`, `restart`, or
    /// `start`. Empty when the surface did not report one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    /// The plan state that motivated the apply: live gateway drift,
    /// whether the gateway was up, and whether a gateway-side change
    /// (or drift) was pending.
    #[serde(default)]
    pub drift: bool,
    #[serde(default)]
    pub gateway_up: bool,
    #[serde(default)]
    pub gateway_pending: bool,
    #[serde(default)]
    pub items: Vec<ActivationExecutionItem>,
}

impl ActivationExecution {
    /// (applied, skipped, failed) item counts for list summaries.
    pub fn outcome_counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for item in &self.items {
            match item.outcome.as_str() {
                "applied" => counts.0 += 1,
                "skipped" => counts.1 += 1,
                "failed" => counts.2 += 1,
                _ => {}
            }
        }
        counts
    }
}

/// The persisted history file: executions oldest → newest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivationHistory {
    #[serde(default)]
    pub executions: Vec<ActivationExecution>,
}

fn history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("activation-history.json")
}

/// Serializes history load-modify-save in this process, for the
/// same reason the activation journal has one: an unsynchronized
/// load-modify-save loses records silently. Poisoning is recovered.
static HISTORY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn history_lock() -> std::sync::MutexGuard<'static, ()> {
    HISTORY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Load the history. A missing or corrupt file is an empty history,
/// never an error: the record is a review aid, and its absence must
/// not break the Activation Plan page (or a recording attempt,
/// which then starts from empty and overwrites the corrupt file
/// with a valid one).
pub fn load_history(data_dir: &Path) -> ActivationHistory {
    match std::fs::read_to_string(history_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ActivationHistory::default(),
    }
}

/// Persist the history (tmp-file + rename, the journal's pattern,
/// so a failed write leaves the previous file intact).
fn save_history(data_dir: &Path, history: &ActivationHistory) -> Result<(), String> {
    let text = serde_json::to_string_pretty(history)
        .map_err(|e| format!("the activation history is not serializable: {e}"))?;
    let path = history_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    Ok(())
}

/// Sequence for server-assigned execution ids (the id also carries
/// the reported start instant, so cross-restart collisions would
/// need two applies started in the same millisecond).
static ID_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn assign_id(execution: &ActivationExecution) -> String {
    let seq = ID_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start_ms = chrono::DateTime::parse_from_rfc3339(&execution.started_at)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
    format!("exec-{start_ms}-{seq}")
}

/// Assign `execution` its id, append it to the history, enforce
/// the retention cap (oldest dropped first), and persist. Returns
/// the assigned id.
///
/// BEST-EFFORT by contract: any failure (serialization, I/O) is
/// swallowed and reported as `None` — recording a finished apply
/// must never fail or alter the activation it describes.
pub fn record_execution(data_dir: &Path, execution: &mut ActivationExecution) -> Option<String> {
    let _guard = history_lock();
    execution.id = assign_id(execution);
    let mut history = load_history(data_dir);
    history.executions.push(execution.clone());
    if history.executions.len() > HISTORY_CAP {
        let excess = history.executions.len() - HISTORY_CAP;
        history.executions.drain(..excess);
    }
    save_history(data_dir, &history).ok()?;
    Some(execution.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activation-history-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An apply-shaped execution: two backend restarts, one
    /// already-effective skip, one gateway change.
    fn sample_execution(status: &str) -> ActivationExecution {
        ActivationExecution {
            id: String::new(),
            started_at: "2026-10-18T12:00:00+00:00".to_string(),
            finished_at: "2026-10-18T12:00:42+00:00".to_string(),
            status: status.to_string(),
            source: "desktop".to_string(),
            mode: "restart".to_string(),
            drift: false,
            gateway_up: true,
            gateway_pending: true,
            items: vec![
                ActivationExecutionItem {
                    ts: "2026-10-18T11:58:00+00:00".to_string(),
                    kind: "backend_flags".to_string(),
                    summary: "Backend `coder` flags changed".to_string(),
                    scope: "backend".to_string(),
                    targets: vec!["coder".to_string()],
                    activation: "restart".to_string(),
                    outcome: "applied".to_string(),
                    detail: None,
                },
                ActivationExecutionItem {
                    ts: "2026-10-18T11:59:00+00:00".to_string(),
                    kind: "backend_set_offload".to_string(),
                    summary: "Backend `planner` expert offload set to 24".to_string(),
                    scope: "backend".to_string(),
                    targets: vec!["planner".to_string()],
                    activation: "next_start".to_string(),
                    outcome: "skipped".to_string(),
                    detail: Some(
                        "Already effective — takes effect at the backend's next start"
                            .to_string(),
                    ),
                },
                ActivationExecutionItem {
                    ts: "2026-10-18T11:59:30+00:00".to_string(),
                    kind: "gateway_config".to_string(),
                    summary: "Gateway configuration changed".to_string(),
                    scope: "gateway".to_string(),
                    targets: Vec::new(),
                    activation: "gateway_restart".to_string(),
                    outcome: "applied".to_string(),
                    detail: None,
                },
            ],
        }
    }

    #[test]
    fn recorded_execution_round_trips_identically() {
        let dir = test_dir("round-trip");
        let mut execution = sample_execution("completed");
        let id = record_execution(&dir, &mut execution).expect("recorded");
        assert!(!id.is_empty());
        assert_eq!(execution.id, id);
        let history = load_history(&dir);
        assert_eq!(history.executions.len(), 1);
        let back = &history.executions[0];
        // Serialize both sides: the read-back record is field-for-
        // field the record that was written, id included.
        assert_eq!(
            serde_json::to_value(back).unwrap(),
            serde_json::to_value(&execution).unwrap()
        );
        assert_eq!(back.outcome_counts(), (2, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_execution_is_recorded_with_its_failures() {
        // The failure path is the point of the feature: an apply
        // that aborted midway still lands in the history, with the
        // failed item's error text and the unattempted items
        // marked skipped.
        let dir = test_dir("failure-path");
        let mut execution = sample_execution("failed");
        execution.items[0].outcome = "failed".to_string();
        execution.items[0].detail = Some("backend restart timed out".to_string());
        execution.items[2].outcome = "skipped".to_string();
        execution.items[2].detail =
            Some("Not attempted — the apply stopped before the gateway restart".to_string());
        assert!(record_execution(&dir, &mut execution).is_some());
        let history = load_history(&dir);
        let back = &history.executions[0];
        assert_eq!(back.status, "failed");
        assert_eq!(back.outcome_counts(), (0, 2, 1));
        assert_eq!(
            back.items[0].detail.as_deref(),
            Some("backend restart timed out")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_cap_drops_the_oldest_first() {
        let dir = test_dir("retention");
        let total = HISTORY_CAP + 5;
        let mut ids = Vec::new();
        for i in 0..total {
            let mut execution = sample_execution("completed");
            execution.started_at = format!("2026-10-18T12:{:02}:00+00:00", i % 60);
            ids.push(record_execution(&dir, &mut execution).expect("recorded"));
        }
        let history = load_history(&dir);
        assert_eq!(history.executions.len(), HISTORY_CAP);
        // The first five are gone; the retained run is exactly the
        // newest HISTORY_CAP, in order.
        let retained: Vec<&str> = history
            .executions
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        let expected: Vec<&str> = ids[5..].iter().map(|s| s.as_str()).collect();
        assert_eq!(retained, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_history_file_degrades_to_empty_and_recovers() {
        let dir = test_dir("corrupt");
        std::fs::write(history_path(&dir), "{ not json").unwrap();
        assert!(load_history(&dir).executions.is_empty());
        // A non-object JSON document degrades the same way.
        std::fs::write(history_path(&dir), "[1, 2, 3]").unwrap();
        assert!(load_history(&dir).executions.is_empty());
        // Recording after corruption starts from empty and leaves
        // a valid file behind.
        let mut execution = sample_execution("completed");
        assert!(record_execution(&dir, &mut execution).is_some());
        let history = load_history(&dir);
        assert_eq!(history.executions.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_history_file_is_an_empty_history() {
        let dir = test_dir("missing");
        assert!(load_history(&dir).executions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_into_an_unwritable_dir_fails_soft() {
        // Best-effort contract: the data dir is a path through a
        // regular file, so every write fails — record_execution
        // must report None, never panic or propagate.
        let dir = test_dir("unwritable");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let mut execution = sample_execution("completed");
        assert!(record_execution(&file, &mut execution).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
