//! API-side operation records for the asynchronous endpoints
//! (rest-api.md §3.1). A request to an AsyncOp route creates a
//! record here, spawns the dispatch, and answers 202 with the
//! record; the record's `detail` carries the outcome the
//! synchronous call would have returned. Records are retained for
//! the process lifetime (decision 14) and merge into
//! `GET /operations` alongside the app's own task records.
//!
//! Abort is bookkeeping, exactly as in the app: the record flips
//! to `aborted` and the underlying work — which the app cannot
//! cancel either — is left to finish detached; its completion does
//! not overwrite the aborted state.
//!
//! The store also retains diagnostics bundle locations: when an
//! export operation completes, its file is registered under a
//! server-assigned bundle id for the content endpoint.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::users::now_ms;

pub struct OperationStore {
    ops: Mutex<HashMap<String, Value>>,
    bundles: Mutex<HashMap<String, PathBuf>>,
}

impl Default for OperationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationStore {
    pub fn new() -> Self {
        Self {
            ops: Mutex::new(HashMap::new()),
            bundles: Mutex::new(HashMap::new()),
        }
    }

    /// Create a running record for `kind`; returns (id, record).
    pub fn begin(&self, kind: &str) -> (String, Value) {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let record = json!({
            "id": id,
            "kind": kind,
            "status": "running",
            "detail": null,
            "created_ms": now,
            "started_ms": now,
            "finished_ms": null,
            "error": null,
        });
        self.ops
            .lock()
            .unwrap()
            .insert(id.clone(), record.clone());
        (id, record)
    }

    /// Record a finished dispatch. `detail` is the value the
    /// synchronous call would have returned. No-op when the record
    /// was aborted meanwhile (abort is bookkeeping; see module
    /// docs).
    pub fn complete(&self, id: &str, kind: &str, result: Result<Value, String>) {
        // A completed diagnostics export registers its bundle.
        let mut detail = match &result {
            Ok(v) => Some(v.clone()),
            Err(_) => None,
        };
        if kind == "diagnostics_export_bundle" {
            if let Some(path) = detail
                .as_ref()
                .and_then(|d| d.get("path"))
                .and_then(Value::as_str)
            {
                let bundle_id = uuid::Uuid::new_v4().to_string();
                self.bundles
                    .lock()
                    .unwrap()
                    .insert(bundle_id.clone(), PathBuf::from(path));
                if let Some(obj) = detail.as_mut().and_then(Value::as_object_mut) {
                    obj.insert("bundle_id".into(), json!(bundle_id));
                }
            }
        }
        let mut ops = self.ops.lock().unwrap();
        let Some(record) = ops.get_mut(id) else { return };
        if record["status"] == "aborted" {
            return;
        }
        record["finished_ms"] = json!(now_ms());
        match result {
            Ok(_) => {
                record["status"] = json!("done");
                record["detail"] = detail.unwrap_or(Value::Null);
            }
            Err(message) => {
                record["status"] = json!("failed");
                record["error"] = json!(message);
            }
        }
    }

    /// Complete a record as failed but keep the operation's own
    /// payload as the detail (the wizard-step busy outcome).
    pub fn fail_with_detail(&self, id: &str, message: &str, detail: Value) {
        let mut ops = self.ops.lock().unwrap();
        let Some(record) = ops.get_mut(id) else { return };
        if record["status"] == "aborted" {
            return;
        }
        record["status"] = json!("failed");
        record["error"] = json!(message);
        record["detail"] = detail;
        record["finished_ms"] = json!(now_ms());
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.ops.lock().unwrap().get(id).cloned()
    }

    /// All records, oldest first (the app's task list order).
    pub fn list(&self) -> Vec<Value> {
        let mut all: Vec<Value> = self.ops.lock().unwrap().values().cloned().collect();
        all.sort_by_key(|r| r["created_ms"].as_i64().unwrap_or(0));
        all
    }

    /// Flip a record to aborted (bookkeeping parity with the app's
    /// task_abort, which sets the status unconditionally). Returns
    /// the record when it exists.
    pub fn abort(&self, id: &str) -> Option<Value> {
        let mut ops = self.ops.lock().unwrap();
        let record = ops.get_mut(id)?;
        record["status"] = json!("aborted");
        if record["finished_ms"].is_null() {
            record["finished_ms"] = json!(now_ms());
        }
        Some(record.clone())
    }

    pub fn bundle_path(&self, bundle_id: &str) -> Option<PathBuf> {
        self.bundles.lock().unwrap().get(bundle_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_abort_parity() {
        let store = OperationStore::new();
        let (id, record) = store.begin("start_backend");
        assert_eq!(record["status"], "running");
        store.complete(&id, "start_backend", Ok(json!({ "ok": true })));
        let done = store.get(&id).unwrap();
        assert_eq!(done["status"], "done");
        assert_eq!(done["detail"]["ok"], true);
        assert!(done["finished_ms"].is_number());

        let (id2, _) = store.begin("download_model");
        let aborted = store.abort(&id2).unwrap();
        assert_eq!(aborted["status"], "aborted");
        // Late completion must not overwrite the aborted state.
        store.complete(&id2, "download_model", Ok(json!({})));
        assert_eq!(store.get(&id2).unwrap()["status"], "aborted");
        assert!(store.abort("nope").is_none());
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn bundle_registration_on_export_completion() {
        let store = OperationStore::new();
        let (id, _) = store.begin("diagnostics_export_bundle");
        store.complete(
            &id,
            "diagnostics_export_bundle",
            Ok(json!({ "path": "/tmp/diag.zip" })),
        );
        let record = store.get(&id).unwrap();
        let bundle_id = record["detail"]["bundle_id"].as_str().unwrap();
        assert_eq!(
            store.bundle_path(bundle_id).unwrap(),
            PathBuf::from("/tmp/diag.zip")
        );
    }
}
