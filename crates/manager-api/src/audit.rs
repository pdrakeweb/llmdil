//! The admin API audit log: `logs/admin-api-audit.jsonl` under the
//! data dir. One JSON record per mutation and per auth event:
//! timestamp, actor, auth type, source IP, action, target, result,
//! request id. Never passwords, tokens, or key material. No API
//! endpoint truncates or edits it; admins read it at
//! `GET /api/v1/admin/audit` (phase 3).

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

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

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join("logs").join("admin-api-audit.jsonl"),
            lock: Mutex::new(()),
        }
    }

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
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                {
                    Ok(mut f) => {
                        if let Err(e) = writeln!(f, "{line}") {
                            eprintln!("admin api audit: write failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("admin api audit: open failed: {e}"),
                }
            }
            Err(e) => eprintln!("admin api audit: serialize failed: {e}"),
        }
    }

    /// Read the newest records first (admin review surface).
    pub fn read_recent(&self, offset: usize, limit: usize) -> Vec<serde_json::Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .rev()
            .skip(offset)
            .take(limit)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_recent() {
        let dir = std::env::temp_dir().join(format!(
            "manager-api-audit-test-{}",
            std::process::id()
        ));
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
}
