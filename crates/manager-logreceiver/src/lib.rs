//! Store for the Prometheus-local LLMdil log receiver: one SQLite
//! file, one `records` table mirroring the shipper's wire format
//! (log-streaming design §6.3/§6.7), deduped on `(machine, seq)`,
//! plus a `heartbeats` table the receiver writes into directly so
//! Muse can tell "receiver up, shipper quiet" from "receiver
//! unreachable" (DESIGN-prometheus-local.md §4).

use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How long heartbeat rows are kept (design §4): 5-minute rows,
/// pruned to 7 days in the hourly prune loop.
pub const HEARTBEAT_RETENTION_DAYS: i64 = 7;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// One stored record (the wire record plus receiver arrival time).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    pub machine: String,
    pub app_version: String,
    pub seq: u64,
    pub ts_wall: String,
    pub ts_mono_ms: u64,
    pub source: String,
    pub stream: String,
    pub level: String,
    pub line: String,
    #[serde(default)]
    pub received_at: String,
}

/// One heartbeat row: proof the receiver process and its store
/// were alive at `ts_utc`, written directly to the store (never
/// over HTTP, so it does not depend on the path it monitors).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub ts_utc: String,
    pub uptime_s: u64,
    pub records: i64,
    pub db_bytes: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Query {
    /// Comma-separated sources; empty = all.
    pub sources: Vec<String>,
    pub level: Option<String>,
    pub grep: Option<String>,
    /// RFC 3339 lower bound on `ts_wall` (lexicographic compare is
    /// valid for the shipper's fixed-offset local stamps within a
    /// deployment; the receiver also stamps `received_at`).
    pub since: Option<String>,
    pub limit: usize,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS records (
        machine     TEXT NOT NULL,
        seq         INTEGER NOT NULL,
        app_version TEXT NOT NULL,
        ts_wall     TEXT NOT NULL,
        ts_utc      TEXT NOT NULL,
        ts_mono_ms  INTEGER NOT NULL,
        source      TEXT NOT NULL,
        stream      TEXT NOT NULL,
        level       TEXT NOT NULL,
        line        TEXT NOT NULL,
        received_at TEXT NOT NULL,
        PRIMARY KEY (machine, seq)
    );
    CREATE INDEX IF NOT EXISTS idx_records_ts ON records(ts_wall);
    CREATE INDEX IF NOT EXISTS idx_records_source ON records(source);
    CREATE INDEX IF NOT EXISTS idx_records_level ON records(level);
    CREATE TABLE IF NOT EXISTS prune_log (
        pruned_at TEXT NOT NULL,
        detail    TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS heartbeats (
        ts_utc   TEXT NOT NULL,
        uptime_s INTEGER NOT NULL,
        records  INTEGER NOT NULL,
        db_bytes INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_heartbeats_ts ON heartbeats(ts_utc);";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Insert a batch, deduping on `(machine, seq)`. Returns
    /// (accepted, duplicates).
    pub fn insert_batch(&mut self, records: &[Record]) -> Result<(usize, usize), StoreError> {
        let tx = self.conn.transaction()?;
        let mut accepted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO records
                 (machine, seq, app_version, ts_wall, ts_utc, ts_mono_ms, source, stream, level, line, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for r in records {
                // Normalized UTC stamp for offset-safe time
                // filters; the operator's local stamp stays in
                // ts_wall for display.
                let ts_utc = chrono::DateTime::parse_from_rfc3339(&r.ts_wall)
                    .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
                    .unwrap_or_else(|_| r.received_at.clone());
                let n = stmt.execute(params![
                    r.machine,
                    r.seq as i64,
                    r.app_version,
                    r.ts_wall,
                    ts_utc,
                    r.ts_mono_ms as i64,
                    r.source,
                    r.stream,
                    r.level,
                    r.line,
                    r.received_at,
                ])?;
                accepted += n;
            }
        }
        tx.commit()?;
        Ok((accepted, records.len() - accepted))
    }

    pub fn query(&self, q: &Query) -> Result<Vec<Record>, StoreError> {
        let mut sql = String::from(
            "SELECT machine, seq, app_version, ts_wall, ts_mono_ms, source, stream, level, line, received_at FROM records WHERE 1=1",
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !q.sources.is_empty() {
            let marks: Vec<&str> = q.sources.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND source IN ({})", marks.join(",")));
            for s in &q.sources {
                args.push(s.clone().into());
            }
        }
        if let Some(level) = &q.level {
            sql.push_str(" AND level = ?");
            args.push(level.clone().into());
        }
        if let Some(grep) = &q.grep {
            sql.push_str(" AND line LIKE ?");
            args.push(format!("%{grep}%").into());
        }
        if let Some(since) = &q.since {
            // `since` is a normalized UTC RFC 3339 bound (the CLI
            // and query endpoint normalize before calling).
            sql.push_str(" AND ts_utc >= ?");
            args.push(since.clone().into());
        }
        sql.push_str(" ORDER BY machine, seq DESC LIMIT ?");
        args.push((q.limit as i64).into());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok(Record {
                v: 1,
                machine: row.get(0)?,
                seq: row.get::<_, i64>(1)? as u64,
                app_version: row.get(2)?,
                ts_wall: row.get(3)?,
                ts_mono_ms: row.get::<_, i64>(4)? as u64,
                source: row.get(5)?,
                stream: row.get(6)?,
                level: row.get(7)?,
                line: row.get(8)?,
                received_at: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        // Present newest-last, matching the query contract (§7.1).
        out.reverse();
        Ok(out)
    }

    pub fn count(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?)
    }

    /// Per-machine status: record count, highest seq, last arrival.
    pub fn machine_status(&self) -> Result<Vec<(String, i64, i64, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT machine, COUNT(*), MAX(seq), MAX(received_at) FROM records GROUP BY machine ORDER BY machine",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Write one heartbeat row stamped now, snapshotting the
    /// current record count and database file size.
    pub fn record_heartbeat(&self, uptime_s: u64, db_bytes: u64) -> Result<Heartbeat, StoreError> {
        self.record_heartbeat_at(&chrono::Utc::now().to_rfc3339(), uptime_s, db_bytes)
    }

    fn record_heartbeat_at(
        &self,
        ts_utc: &str,
        uptime_s: u64,
        db_bytes: u64,
    ) -> Result<Heartbeat, StoreError> {
        let records = self.count()?;
        self.conn.execute(
            "INSERT INTO heartbeats (ts_utc, uptime_s, records, db_bytes) VALUES (?1, ?2, ?3, ?4)",
            params![ts_utc, uptime_s as i64, records, db_bytes as i64],
        )?;
        Ok(Heartbeat {
            ts_utc: ts_utc.to_string(),
            uptime_s,
            records,
            db_bytes,
        })
    }

    /// The most recent heartbeat, if any.
    pub fn last_heartbeat(&self) -> Result<Option<Heartbeat>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT ts_utc, uptime_s, records, db_bytes FROM heartbeats ORDER BY ts_utc DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(Heartbeat {
                ts_utc: row.get(0)?,
                uptime_s: row.get::<_, i64>(1)? as u64,
                records: row.get(2)?,
                db_bytes: row.get::<_, i64>(3)? as u64,
            })),
            None => Ok(None),
        }
    }

    /// Retention (design D8, cap per DESIGN-prometheus-local.md
    /// §3.3): drop records older than `days`, then, while the
    /// database file exceeds `cap_bytes`, drop the oldest records
    /// in chunks. Heartbeats older than
    /// [`HEARTBEAT_RETENTION_DAYS`] go in the same pass. Every prune
    /// is written to `prune_log` (§5.4: the prune log states what
    /// was dropped). Returns a human-readable summary.
    pub fn prune(
        &mut self,
        db_path: &Path,
        days: i64,
        cap_bytes: u64,
    ) -> Result<String, StoreError> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let cutoff_s = cutoff.to_rfc3339();
        let mut notes: Vec<String> = Vec::new();
        let n = self.conn.execute(
            "DELETE FROM records WHERE received_at < ?1",
            params![cutoff_s],
        )?;
        if n > 0 {
            notes.push(format!("{n} record(s) older than {days} day(s)"));
        }
        let hb_cutoff =
            (chrono::Utc::now() - chrono::Duration::days(HEARTBEAT_RETENTION_DAYS)).to_rfc3339();
        let n = self.conn.execute(
            "DELETE FROM heartbeats WHERE ts_utc < ?1",
            params![hb_cutoff],
        )?;
        if n > 0 {
            notes.push(format!(
                "{n} heartbeat(s) older than {HEARTBEAT_RETENTION_DAYS} day(s)"
            ));
        }
        loop {
            let size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
            if size <= cap_bytes {
                break;
            }
            let n = self.conn.execute(
                "DELETE FROM records WHERE rowid IN (SELECT rowid FROM records ORDER BY received_at, machine, seq LIMIT 1000)",
                [],
            )?;
            if n == 0 {
                break;
            }
            notes.push(format!(
                "{n} oldest record(s) over the {} MiB cap",
                cap_bytes / 1_048_576
            ));
        }
        if !notes.is_empty() {
            let detail = notes.join("; ");
            self.conn.execute(
                "INSERT INTO prune_log (pruned_at, detail) VALUES (?1, ?2)",
                params![chrono::Utc::now().to_rfc3339(), detail],
            )?;
            self.conn.execute_batch("VACUUM")?;
            return Ok(detail);
        }
        Ok("nothing to prune".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(machine: &str, seq: u64, source: &str, line: &str) -> Record {
        Record {
            v: 1,
            machine: machine.into(),
            app_version: "0.1.50".into(),
            seq,
            ts_wall: "2026-10-23T00:00:00-04:00".into(),
            ts_mono_ms: seq * 10,
            source: source.into(),
            stream: "stdout".into(),
            level: "info".into(),
            line: line.into(),
            received_at: "2026-10-23T04:00:00Z".into(),
        }
    }

    #[test]
    fn insert_dedupes_and_queries() {
        let mut store = Store::open_in_memory().unwrap();
        let batch = vec![
            rec("prometheus", 1, "wizard", "one"),
            rec("prometheus", 2, "wizard", "two"),
        ];
        assert_eq!(store.insert_batch(&batch).unwrap(), (2, 0));
        // Replay: all duplicates.
        assert_eq!(store.insert_batch(&batch).unwrap(), (0, 2));
        let all = store
            .query(&Query {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].line, "one"); // newest last
        let filtered = store
            .query(&Query {
                grep: Some("two".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn heartbeats_record_and_read_back_latest() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.last_heartbeat().unwrap().is_none());
        store
            .record_heartbeat_at("2026-10-23T04:00:00Z", 60, 4096)
            .unwrap();
        let hb = store
            .record_heartbeat_at("2026-10-23T04:05:00Z", 360, 8192)
            .unwrap();
        assert_eq!(hb.records, 0);
        let last = store.last_heartbeat().unwrap().expect("one heartbeat");
        assert_eq!(last.ts_utc, "2026-10-23T04:05:00Z");
        assert_eq!(last.uptime_s, 360);
        assert_eq!(last.db_bytes, 8192);
    }
}
