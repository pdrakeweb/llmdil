//! API-side operation records for the asynchronous endpoints
//! (rest-api.md §3.1). A request to an AsyncOp route creates a
//! record here, spawns the dispatch, and answers 202 with the
//! record; the record's `detail` carries the outcome the
//! synchronous call would have returned. Records are retained for
//! the process lifetime (decision 14) and merge into
//! `GET /operations` alongside the app's own task records.
//!
//! A record knows the app tasks its dispatch recorded
//! ([`TaskLink`]): its `task_id` names the first of them, and an
//! abort is routed to them, which cancels the work cooperatively
//! (`manager-app`'s `tasks` module). Work without a task cannot be
//! cancelled: the record is then marked `aborted` with an
//! `abort_note` and what the work eventually did is kept as
//! `late_outcome`. A record that reached a terminal state is never
//! rewritten.
//!
//! The store also retains diagnostics bundle locations: when an
//! export operation completes, its file is registered under a
//! server-assigned bundle id for the content endpoint.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use manager_app::TaskLink;
use serde_json::{json, Value};

use crate::users::now_ms;

/// Why an operation failed, with the machine-readable error code a
/// synchronous route would have answered.
#[derive(Debug, Clone, PartialEq)]
pub struct OpFailure {
    pub message: String,
    pub code: &'static str,
    pub kind: &'static str,
    /// The operation's own payload, when it returned one.
    pub detail: Option<Value>,
}

/// How an operation ended.
#[derive(Debug, Clone, PartialEq)]
pub enum OpOutcome {
    Done(Value),
    Failed(OpFailure),
    /// Its app task was aborted.
    Aborted(Option<Value>),
}

/// What an abort request found.
pub enum AbortTarget {
    /// Already terminal, with this status; nothing to do.
    Finished(String),
    /// Running, with the app tasks to cancel.
    Running(Arc<TaskLink>),
}

struct Entry {
    record: Value,
    link: Arc<TaskLink>,
    abort_requested: bool,
}

impl Entry {
    fn running(&self) -> bool {
        self.record["status"] == "running"
    }

    fn view(&self) -> Value {
        let mut record = self.record.clone();
        record["task_id"] = json!(self.link.task_ids().first());
        record
    }
}

/// In-memory records for the asynchronous endpoints plus exported diagnostics bundles.
/// Process-lifetime only: nothing is persisted and records are never evicted.
pub struct OperationStore {
    ops: Mutex<HashMap<String, Entry>>,
    bundles: Mutex<HashMap<String, PathBuf>>,
}

impl Default for OperationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationStore {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            ops: Mutex::new(HashMap::new()),
            bundles: Mutex::new(HashMap::new()),
        }
    }

    /// Create a running record for `kind`; returns (id, record, the link
    /// the dispatch runs under).
    pub fn begin(&self, kind: &str) -> (String, Value, Arc<TaskLink>) {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let entry = Entry {
            record: json!({
                "id": id,
                "kind": kind,
                "status": "running",
                "detail": null,
                "created_ms": now,
                "started_ms": now,
                "finished_ms": null,
                "error": null,
                "error_code": null,
                "error_kind": null,
            }),
            link: Arc::new(TaskLink::default()),
            abort_requested: false,
        };
        let (view, link) = (entry.view(), Arc::clone(&entry.link));
        self.ops.lock().unwrap().insert(id.clone(), entry);
        (id, view, link)
    }

    /// Record a finished dispatch. A record that was marked aborted
    /// meanwhile keeps that state; a failure after an abort request is
    /// the abort.
    pub fn complete(&self, id: &str, kind: &str, outcome: OpOutcome) {
        let outcome = match outcome {
            OpOutcome::Done(detail) if kind == "diagnostics_export_bundle" => {
                OpOutcome::Done(self.register_bundle(detail))
            }
            other => other,
        };
        let mut ops = self.ops.lock().unwrap();
        let Some(entry) = ops.get_mut(id) else {
            return;
        };
        if !entry.running() {
            let late = match outcome {
                OpOutcome::Done(v) => json!({ "status": "done", "detail": v }),
                OpOutcome::Failed(f) => {
                    json!({ "status": "failed", "error": f.message, "detail": f.detail })
                }
                OpOutcome::Aborted(v) => json!({ "status": "aborted", "detail": v }),
            };
            if let Some(detail) = entry.record["detail"].as_object_mut() {
                detail.insert("late_outcome".into(), late);
            }
            return;
        }
        let outcome = match outcome {
            OpOutcome::Failed(f) if entry.abort_requested => OpOutcome::Aborted(f.detail),
            other => other,
        };
        let record = &mut entry.record;
        record["finished_ms"] = json!(now_ms());
        match outcome {
            OpOutcome::Done(detail) => {
                record["status"] = json!("done");
                record["detail"] = detail;
            }
            OpOutcome::Failed(f) => {
                record["status"] = json!("failed");
                record["error"] = json!(f.message);
                record["error_code"] = json!(f.code);
                record["error_kind"] = json!(f.kind);
                record["detail"] = json!(f.detail);
            }
            OpOutcome::Aborted(detail) => {
                record["status"] = json!("aborted");
                record["detail"] = json!(detail);
            }
        }
    }

    /// A completed diagnostics export registers its bundle.
    fn register_bundle(&self, mut detail: Value) -> Value {
        if let Some(path) = detail.get("path").and_then(Value::as_str) {
            let path = PathBuf::from(path);
            let bundle_id = uuid::Uuid::new_v4().to_string();
            self.bundles.lock().unwrap().insert(bundle_id.clone(), path);
            detail["bundle_id"] = json!(bundle_id);
        }
        detail
    }

    /// Snapshot of one record by id.
    pub fn get(&self, id: &str) -> Option<Value> {
        self.ops.lock().unwrap().get(id).map(Entry::view)
    }

    /// All records with the ids of all their app tasks, in no order.
    pub fn list(&self) -> Vec<(Value, Vec<String>)> {
        self.ops
            .lock()
            .unwrap()
            .values()
            .map(|e| (e.view(), e.link.task_ids()))
            .collect()
    }

    /// Link a task the operation refers to but did not record itself.
    pub fn link_task(&self, id: &str, task_id: &str) {
        if let Some(entry) = self.ops.lock().unwrap().get(id) {
            entry.link.add(task_id);
        }
    }

    /// The id of the record an app task belongs to.
    pub fn operation_of_task(&self, task_id: &str) -> Option<String> {
        self.ops
            .lock()
            .unwrap()
            .iter()
            .find(|(_, e)| e.link.task_ids().iter().any(|t| t == task_id))
            .map(|(id, _)| id.clone())
    }

    /// Note an abort request; `None` when there is no such record.
    pub fn request_abort(&self, id: &str) -> Option<AbortTarget> {
        let mut ops = self.ops.lock().unwrap();
        let entry = ops.get_mut(id)?;
        if !entry.running() {
            let status = entry.record["status"].as_str().unwrap_or_default();
            return Some(AbortTarget::Finished(status.to_string()));
        }
        entry.abort_requested = true;
        Some(AbortTarget::Running(Arc::clone(&entry.link)))
    }

    /// Mark a record aborted whose work did not (or cannot) stop. A
    /// record that settled meanwhile keeps its state.
    pub fn force_abort(&self, id: &str, note: &str) {
        let mut ops = self.ops.lock().unwrap();
        let Some(entry) = ops.get_mut(id).filter(|e| e.running()) else {
            return;
        };
        entry.record["status"] = json!("aborted");
        entry.record["finished_ms"] = json!(now_ms());
        entry.record["detail"] = json!({ "abort_forced": true, "abort_note": note });
    }

    /// The file a completed diagnostics export registered under `bundle_id`. Ids are
    /// server-assigned UUIDs, so a client can never name an arbitrary path.
    pub fn bundle_path(&self, bundle_id: &str) -> Option<PathBuf> {
        self.bundles.lock().unwrap().get(bundle_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> OpFailure {
        OpFailure {
            message: "no".into(),
            code: "BUSY",
            kind: "Busy",
            detail: Some(json!({ "busy": true })),
        }
    }

    #[test]
    fn lifecycle_and_failure_fields() {
        let store = OperationStore::new();
        let (id, record, link) = store.begin("start_backend");
        assert_eq!(record["status"], "running");
        assert!(record["task_id"].is_null());
        link.add("task-000001");
        link.add("task-000002");
        store.complete(&id, "start_backend", OpOutcome::Done(json!({ "ok": true })));
        let done = store.get(&id).unwrap();
        assert_eq!(done["status"], "done");
        assert_eq!(done["detail"]["ok"], true);
        assert_eq!(done["task_id"], "task-000001");
        assert!(done["finished_ms"].is_number());
        assert_eq!(store.operation_of_task("task-000002"), Some(id));

        let (id, ..) = store.begin("gateway_restart");
        store.complete(&id, "gateway_restart", OpOutcome::Failed(failure()));
        let failed = store.get(&id).unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"], "no");
        assert_eq!(failed["error_code"], "BUSY");
        assert_eq!(failed["error_kind"], "Busy");
        assert_eq!(failed["detail"]["busy"], true);
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn abort_never_rewrites_a_finished_record() {
        let store = OperationStore::new();
        let (id, ..) = store.begin("start_backend");
        store.complete(&id, "start_backend", OpOutcome::Done(json!({})));
        let before = store.get(&id).unwrap();
        assert!(matches!(
            store.request_abort(&id),
            Some(AbortTarget::Finished(s)) if s == "done"
        ));
        store.force_abort(&id, "late");
        assert_eq!(store.get(&id).unwrap(), before);
        assert!(store.request_abort("nope").is_none());

        // The work failed because it was aborted.
        let (id, ..) = store.begin("run_wizard_step");
        assert!(matches!(
            store.request_abort(&id),
            Some(AbortTarget::Running(_))
        ));
        store.complete(&id, "run_wizard_step", OpOutcome::Failed(failure()));
        let aborted = store.get(&id).unwrap();
        assert_eq!(aborted["status"], "aborted");
        assert!(aborted["error"].is_null());

        // Work that finished anyway is done.
        let (id, ..) = store.begin("run_wizard_step");
        store.request_abort(&id);
        store.complete(&id, "run_wizard_step", OpOutcome::Done(json!({})));
        assert_eq!(store.get(&id).unwrap()["status"], "done");

        // Work that cannot be stopped: aborted, with what it did later.
        let (id, ..) = store.begin("diagnostics_export_bundle");
        store.request_abort(&id);
        store.force_abort(&id, "may still be running");
        store.complete(
            &id,
            "diagnostics_export_bundle",
            OpOutcome::Done(json!({ "x": 1 })),
        );
        let forced = store.get(&id).unwrap();
        assert_eq!(forced["status"], "aborted");
        assert_eq!(forced["detail"]["abort_note"], "may still be running");
        assert_eq!(forced["detail"]["late_outcome"]["detail"]["x"], 1);
    }

    #[test]
    fn bundle_registration_on_export_completion() {
        let store = OperationStore::new();
        let (id, ..) = store.begin("diagnostics_export_bundle");
        store.complete(
            &id,
            "diagnostics_export_bundle",
            OpOutcome::Done(json!({ "path": "/tmp/diag.zip" })),
        );
        let record = store.get(&id).unwrap();
        let bundle_id = record["detail"]["bundle_id"].as_str().unwrap();
        assert_eq!(
            store.bundle_path(bundle_id).unwrap(),
            PathBuf::from("/tmp/diag.zip")
        );
    }
}
