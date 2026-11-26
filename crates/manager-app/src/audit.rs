//! The one writer of `logs/admin-api-audit.jsonl`.
//!
//! The admin API's request-level audit (manager-api `AuditLog`) and
//! the debug console's action records (who ran which resolved command
//! line, from the UI or which API principal) share the file. Both
//! append through [`append_line`], serialized by one process-wide
//! lock and written as a single `write_all` per record, so lines from
//! the two writers never interleave.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

static AUDIT_LOCK: Mutex<()> = Mutex::new(());

/// The audit log path under `data_dir`.
pub fn audit_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("admin-api-audit.jsonl")
}

/// Append one serialized JSON line to `path`. Best-effort: a failure
/// is reported on stderr, never into the caller's result.
pub fn append_line(path: &Path, line: &str) {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write as _;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(buf.as_bytes()) {
                eprintln!("audit: write failed: {e}");
            }
        }
        Err(e) => eprintln!("audit: open failed: {e}"),
    }
}

/// Append one record (an object) to the data dir's audit log.
pub fn append(data_dir: &Path, record: &Value) {
    match serde_json::to_string(record) {
        Ok(line) => append_line(&audit_path(data_dir), &line),
        Err(e) => eprintln!("audit: serialize failed: {e}"),
    }
}
