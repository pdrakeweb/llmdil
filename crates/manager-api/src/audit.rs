//! The admin API audit log: `logs/admin-api-audit.jsonl` under the
//! data dir. One JSON record per mutation and per auth event:
//! timestamp, actor, auth type, source IP, action, target, result,
//! request id. Never passwords, tokens, or key material. No API
//! endpoint truncates or edits it; admins read it at
//! `GET /api/v1/admin/audit` (phase 3).
//!
//! Size-bounded (SEC-05 follow-up): past [`ROTATE_BYTES`] the file
//! rotates to `<file>.prev` — the sibling convention the app's other
//! logs use and the log shipper drains — and older archives shift to
//! `<file>.prev.2` … `<file>.prev.<ARCHIVES>`; the oldest is dropped.
//! The entry being written always lands (in the fresh file, or in the
//! current one if rotation fails). Reads span the archives.
//!
//! Desktop admin actions (SEC-13) land in the same file through
//! [`record_local`]: actor `desktop`, auth type `local`, source IP
//! `local`. Every writer in the process goes through [`AuditLog::shared`],
//! so the API server and the desktop shell serialize on one mutex and
//! rotate one file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Actor recorded for actions taken in the desktop app (SEC-13).
pub const LOCAL_ACTOR: &str = "desktop";
/// Auth type recorded for desktop actions: the operator is the local
/// administrator, authenticated by the OS session, not by the API.
pub const LOCAL_AUTH_TYPE: &str = "local";

/// Rotate the live file once appending would take it past this size
/// (~1 MiB, matching the wizard and gateway logs).
pub const ROTATE_BYTES: u64 = 1_048_576;
/// Rotated archives kept (`.prev`, `.prev.2`, … `.prev.N`).
pub const ARCHIVES: usize = 4;

use serde::Serialize;

/// One audit-log line. Identities and outcomes only — never passwords, tokens or key
/// material. `actor` is attacker-chosen text for failed logins, so callers cap its length.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts_ms: i64,
    pub actor: String,
    pub auth_type: String,
    pub source_ip: String,
    pub action: String,
    pub target: Option<String>,
    pub result: String,
    pub request_id: String,
}

/// Append-only JSONL audit log. Appends are serialized by an in-process mutex so concurrent
/// requests never interleave partial lines; nothing in the API rewrites or truncates it.
/// Size rotation moves whole files aside (see the module docs); no record is edited.
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
    rotate_bytes: u64,
}

impl AuditLog {
    /// Log at `<data_dir>/logs/admin-api-audit.jsonl`. Touches no files: the directory and
    /// file are created on the first append.
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join("logs").join("admin-api-audit.jsonl"),
            lock: Mutex::new(()),
            rotate_bytes: ROTATE_BYTES,
        }
    }

    /// The process-wide log for `data_dir`. Every writer (the API
    /// server across restarts, the desktop shell) shares this one
    /// instance, so appends never interleave and only one writer
    /// rotates the file. Touches no files.
    pub fn shared(data_dir: &Path) -> Arc<Self> {
        static LOGS: OnceLock<Mutex<HashMap<PathBuf, Arc<AuditLog>>>> = OnceLock::new();
        let log = Self::new(data_dir);
        let mut logs = LOGS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        logs.entry(log.path.clone())
            .or_insert_with(|| Arc::new(log))
            .clone()
    }

    /// Archive `n` (1-based): `<file>.prev`, then `<file>.prev.<n>`.
    fn archive_path(&self, n: usize) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(".prev");
        if n > 1 {
            s.push(format!(".{n}"));
        }
        PathBuf::from(s)
    }

    /// Rotate when appending `incoming` bytes would pass the limit.
    /// Errors are reported and the append proceeds on the current
    /// file: rotation must never cost an audit record.
    fn rotate_if_needed(&self, incoming: u64) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() == 0 || meta.len() + incoming <= self.rotate_bytes {
            return;
        }
        let _ = std::fs::remove_file(self.archive_path(ARCHIVES));
        for n in (1..ARCHIVES).rev() {
            let from = self.archive_path(n);
            if from.exists() {
                if let Err(e) = std::fs::rename(&from, self.archive_path(n + 1)) {
                    tracing::warn!("admin api audit: archive shift failed: {e}");
                }
            }
        }
        if let Err(e) = std::fs::rename(&self.path, self.archive_path(1)) {
            tracing::warn!("admin api audit: rotation failed: {e}");
        }
    }

    /// The live file, then the archives newest to oldest.
    fn files_newest_first(&self) -> Vec<PathBuf> {
        std::iter::once(self.path.clone())
            .chain((1..=ARCHIVES).map(|n| self.archive_path(n)))
            .collect()
    }

    /// The JSONL file this log appends to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append one record. Best-effort: an audit write failure is
    /// reported on stderr, never failed into the request path — but
    /// it is never silently dropped either.
    pub fn append(&self, entry: &AuditEntry) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string(entry) {
            Ok(line) => {
                use std::io::Write;
                self.rotate_if_needed(line.len() as u64 + 1);
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                {
                    Ok(mut f) => {
                        if let Err(e) = writeln!(f, "{line}") {
                            tracing::warn!("admin api audit: write failed: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("admin api audit: open failed: {e}"),
                }
            }
            Err(e) => tracing::warn!("admin api audit: serialize failed: {e}"),
        }
    }

    /// Read the newest records first (admin review surface), across
    /// the live file and its archives; files beyond what `offset` +
    /// `limit` needs are not read.
    pub fn read_recent(&self, offset: usize, limit: usize) -> Vec<serde_json::Value> {
        let mut skip = offset;
        let mut out = Vec::new();
        for path in self.files_newest_first() {
            if out.len() >= limit {
                break;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines().rev() {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if out.len() >= limit {
                    break;
                }
                if let Ok(v) = serde_json::from_str(line) {
                    out.push(v);
                }
            }
        }
        out
    }
}

/// The entry for a desktop admin action (SEC-13): actor
/// [`LOCAL_ACTOR`], auth type [`LOCAL_AUTH_TYPE`], result `ok` or
/// `error`. Only the action name and the target identity are taken
/// from the caller, so no password, temporary password, enrollment
/// code or key material can reach the log through this path.
pub fn local_entry(action: &str, target: Option<&str>, ok: bool) -> AuditEntry {
    AuditEntry {
        ts_ms: crate::users::now_ms(),
        actor: LOCAL_ACTOR.to_string(),
        auth_type: LOCAL_AUTH_TYPE.to_string(),
        source_ip: "local".to_string(),
        action: action.to_string(),
        // Bounded like every other identity field (SEC-05).
        target: target.map(|t| t.chars().take(crate::auth::MAX_USERNAME_LEN).collect()),
        result: if ok { "ok" } else { "error" }.to_string(),
        request_id: uuid::Uuid::new_v4().to_string(),
    }
}

/// Audit a desktop admin action into `<data_dir>`'s Admin API audit
/// log — the same file, format and rotation as API entries, written
/// whether or not the API server is running. `action` matches the
/// API's action string where the API has the same operation (e.g.
/// `DELETE /api/v1/admin/users/{id}`), so one action reads the same
/// from either surface. The outcome's value and error are not
/// recorded, only whether it succeeded.
pub fn record_local<T, E>(
    data_dir: &Path,
    action: &str,
    target: Option<&str>,
    outcome: &Result<T, E>,
) {
    AuditLog::shared(data_dir).append(&local_entry(action, target, outcome.is_ok()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "manager-api-audit-{tag}-{}-{}",
            std::process::id(),
            crate::users::generate_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// SEC-13: a desktop action is one entry with exactly the standard
    /// fields, actor `desktop`, auth type `local`, `ok`/`error` result.
    #[test]
    fn record_local_writes_desktop_entry() {
        let dir = temp_dir("local");
        let ok: Result<serde_json::Value, String> =
            Ok(serde_json::json!({ "temporary_password": "s3cret-temp" }));
        record_local(
            &dir,
            "POST /api/v1/admin/users/{id}/reset-password",
            Some("user-1"),
            &ok,
        );
        let err: Result<(), String> = Err("enrollment code ABCD-1234 rejected".into());
        record_local(
            &dir,
            "POST /api/v1/admin/enrollment-codes",
            Some("user-2"),
            &err,
        );

        let text = std::fs::read_to_string(dir.join("logs").join("admin-api-audit.jsonl")).unwrap();
        // Neither the success value nor the error text reaches the log.
        assert!(!text.contains("s3cret-temp"));
        assert!(!text.contains("ABCD-1234"));
        assert!(!text.contains("temporary_password"));

        let recent = AuditLog::shared(&dir).read_recent(0, 10);
        assert_eq!(recent.len(), 2);
        let mut keys: Vec<&str> = recent[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "action",
                "actor",
                "auth_type",
                "request_id",
                "result",
                "source_ip",
                "target",
                "ts_ms"
            ]
        );
        assert_eq!(recent[0]["actor"], "desktop");
        assert_eq!(recent[0]["auth_type"], "local");
        assert_eq!(recent[0]["source_ip"], "local");
        assert_eq!(recent[0]["action"], "POST /api/v1/admin/enrollment-codes");
        assert_eq!(recent[0]["target"], "user-2");
        assert_eq!(recent[0]["result"], "error");
        assert_eq!(recent[1]["result"], "ok");
        assert_eq!(recent[1]["target"], "user-1");
        assert_ne!(recent[0]["request_id"], recent[1]["request_id"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One log instance per data dir, so the API server and the
    /// desktop shell serialize on the same mutex.
    #[test]
    fn shared_is_one_instance_per_data_dir() {
        let a = temp_dir("shared-a");
        let b = temp_dir("shared-b");
        assert!(Arc::ptr_eq(&AuditLog::shared(&a), &AuditLog::shared(&a)));
        assert!(!Arc::ptr_eq(&AuditLog::shared(&a), &AuditLog::shared(&b)));
    }

    /// Desktop entries go through the same size rotation as API ones.
    #[test]
    fn record_local_rotates() {
        let dir = temp_dir("local-rotate");
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let live = logs.join("admin-api-audit.jsonl");
        let filler = serde_json::to_string(&entry(0)).unwrap() + "\n";
        let lines = (ROTATE_BYTES as usize / filler.len()) + 1;
        std::fs::write(&live, filler.repeat(lines)).unwrap();
        record_local(&dir, "tls_cert_regenerate", None, &Ok::<(), ()>(()));
        assert!(logs.join("admin-api-audit.jsonl.prev").exists());
        assert!(std::fs::metadata(&live).unwrap().len() <= ROTATE_BYTES);
        let newest = AuditLog::shared(&dir).read_recent(0, 1);
        assert_eq!(newest[0]["action"], "tls_cert_regenerate");
        assert_eq!(newest[0]["target"], serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_and_read_recent() {
        let dir =
            std::env::temp_dir().join(format!("manager-api-audit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = AuditLog::new(&dir);
        for i in 0..3 {
            log.append(&AuditEntry {
                ts_ms: i,
                actor: "alice".into(),
                auth_type: "session".into(),
                source_ip: "127.0.0.1".into(),
                action: "POST /api/v1/backends/{id}/start".into(),
                target: Some("planner".into()),
                result: "202".into(),
                request_id: format!("req-{i}"),
            });
        }
        let recent = log.read_recent(0, 2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["request_id"], "req-2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entry(i: usize) -> AuditEntry {
        AuditEntry {
            // Fixed width, so every line has the same length.
            ts_ms: 1_000_000 + i as i64,
            actor: "alice".into(),
            auth_type: "session".into(),
            source_ip: "127.0.0.1".into(),
            action: "POST /api/v1/backends/{id}/start".into(),
            target: Some("planner".into()),
            result: "202".into(),
            request_id: format!("req-{i:05}"),
        }
    }

    /// SEC-05 follow-up: the log rotates by size into `.prev`,
    /// `.prev.2` … `.prev.ARCHIVES`, never exceeding the limit per
    /// file, never losing the entry being written, and reads page
    /// newest-first across the archives.
    #[test]
    fn rotates_by_size_and_reads_across_archives() {
        let dir = std::env::temp_dir().join(format!(
            "manager-api-audit-rotate-{}-{}",
            std::process::id(),
            crate::users::generate_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = AuditLog::new(&dir);
        let line_len = serde_json::to_string(&entry(0)).unwrap().len() as u64 + 1;
        // Ten lines per file.
        log.rotate_bytes = line_len * 10;
        let total = 10 * (ARCHIVES + 3) + 4;
        for i in 0..total {
            log.append(&entry(i));
            // The entry just written is always the newest readable one.
            let newest = log.read_recent(0, 1);
            assert_eq!(newest[0]["request_id"], format!("req-{i:05}"));
        }
        for n in 1..=ARCHIVES {
            let p = log.archive_path(n);
            assert!(p.exists(), "{} missing", p.display());
            assert!(std::fs::metadata(&p).unwrap().len() <= log.rotate_bytes);
        }
        assert!(!log.archive_path(ARCHIVES + 1).exists());
        assert!(std::fs::metadata(log.path()).unwrap().len() <= log.rotate_bytes);
        assert_eq!(
            log.archive_path(1).file_name().unwrap(),
            "admin-api-audit.jsonl.prev"
        );
        // Everything retained reads back newest-first, contiguously.
        let all = log.read_recent(0, usize::MAX);
        let retained = 4 + 10 * ARCHIVES;
        assert_eq!(all.len(), retained);
        for (k, v) in all.iter().enumerate() {
            assert_eq!(v["request_id"], format!("req-{:05}", total - 1 - k));
        }
        // Paging crosses file boundaries.
        let page = log.read_recent(3, 5);
        assert_eq!(page.len(), 5);
        assert_eq!(page[0]["request_id"], format!("req-{:05}", total - 4));
        assert_eq!(page[4]["request_id"], format!("req-{:05}", total - 8));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
