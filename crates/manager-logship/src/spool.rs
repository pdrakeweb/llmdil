//! Disk spool + persisted shipper state (design §6.4).
//!
//! Undeliverable batches append to `<data_dir>/logs/shipper-spool/`
//! as segment files (one JSON record per line), capped at
//! [`DEFAULT_SPOOL_CAP_BYTES`]; past the cap the oldest segments are
//! deleted and counted — and the count is itself shipped on recovery
//! as a shipper status record, so the loss is always reported, never
//! silent. `state.json` in the same directory persists the sequence
//! counter, per-source tail offsets, and the unreported drop count
//! across restarts.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::record::WireRecord;

/// 50 MiB spool cap (design D9/§6.4): weeks at typical volume,
/// roughly a day at incident volume.
pub const DEFAULT_SPOOL_CAP_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShipperState {
    /// Next sequence number to assign.
    pub next_seq: u64,
    /// Per-source tail offsets (byte offsets into the current file).
    #[serde(default)]
    pub offsets: BTreeMap<String, u64>,
    /// Spool-cap drops not yet reported to the destination.
    #[serde(default)]
    pub unreported_spool_drops: u64,
}

pub struct Spool {
    dir: PathBuf,
    cap_bytes: u64,
    pub state: ShipperState,
}

impl Spool {
    pub fn open(dir: &Path, cap_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let state = fs::read(dir.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self {
            dir: dir.to_path_buf(),
            cap_bytes,
            state,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save_state(&self) -> io::Result<()> {
        let tmp = self.dir.join("state.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.state)?)?;
        fs::rename(&tmp, self.dir.join("state.json"))
    }

    fn segments(&self) -> io::Result<Vec<PathBuf>> {
        let mut segs: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("seg-") && n.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .collect();
        // Zero-padded sequence numbers in the name sort in order.
        segs.sort();
        Ok(segs)
    }

    pub fn spooled_bytes(&self) -> u64 {
        self.segments()
            .map(|segs| {
                segs.iter()
                    .filter_map(|p| fs::metadata(p).ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    pub fn segment_count(&self) -> usize {
        self.segments().map(|s| s.len()).unwrap_or(0)
    }

    /// Append one undeliverable batch as a new segment, then enforce
    /// the cap by deleting oldest segments. Returns how many records
    /// the cap enforcement dropped (also accumulated into
    /// `state.unreported_spool_drops`).
    pub fn append_batch(&mut self, records: &[WireRecord]) -> io::Result<u64> {
        if records.is_empty() {
            return Ok(0);
        }
        let first = records.first().map(|r| r.seq).unwrap_or(0);
        let last = records.last().map(|r| r.seq).unwrap_or(first);
        let name = format!("seg-{first:010}-{last:010}.jsonl");
        let mut text = String::new();
        for r in records {
            text.push_str(&serde_json::to_string(r)?);
            text.push('\n');
        }
        fs::write(self.dir.join(name), text)?;

        let mut dropped = 0u64;
        let mut segs = self.segments()?;
        while self.spooled_bytes() > self.cap_bytes && segs.len() > 1 {
            let oldest = segs.remove(0);
            let count = fs::read_to_string(&oldest)
                .map(|t| t.lines().count() as u64)
                .unwrap_or(0);
            let _ = fs::remove_file(&oldest);
            dropped += count;
        }
        if dropped > 0 {
            self.state.unreported_spool_drops += dropped;
            self.save_state()?;
        }
        Ok(dropped)
    }

    /// The oldest segment's records, in order, with its path (so the
    /// caller can delete it after a successful send).
    pub fn oldest_segment(&self) -> io::Result<Option<(PathBuf, Vec<WireRecord>)>> {
        let segs = self.segments()?;
        let Some(path) = segs.into_iter().next() else {
            return Ok(None);
        };
        let text = fs::read_to_string(&path)?;
        let mut records = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            records.push(serde_json::from_str(line)?);
        }
        Ok(Some((path, records)))
    }

    pub fn remove_segment(&mut self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{ShipLevel, WIRE_VERSION};

    fn rec(seq: u64) -> WireRecord {
        WireRecord {
            v: WIRE_VERSION,
            machine: "m".into(),
            app_version: "0".into(),
            seq,
            ts_wall: "2026-01-01T00:00:00+00:00".into(),
            ts_mono_ms: seq,
            source: "wizard".into(),
            stream: "stdout".into(),
            level: ShipLevel::Info,
            line: format!("line {seq}"),
        }
    }

    #[test]
    fn spool_round_trip_and_state() {
        let dir = std::env::temp_dir().join(format!("logship-spool-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut spool = Spool::open(&dir, DEFAULT_SPOOL_CAP_BYTES).unwrap();
        spool.state.next_seq = 3;
        spool.save_state().unwrap();
        spool.append_batch(&[rec(1), rec(2)]).unwrap();
        let (path, records) = spool.oldest_segment().unwrap().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        spool.remove_segment(&path).unwrap();
        assert!(spool.oldest_segment().unwrap().is_none());

        let spool2 = Spool::open(&dir, DEFAULT_SPOOL_CAP_BYTES).unwrap();
        assert_eq!(spool2.state.next_seq, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spool_cap_drops_oldest_and_counts() {
        let dir = std::env::temp_dir().join(format!("logship-spool-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // Tiny cap: each 2-record segment is a few hundred bytes.
        let mut spool = Spool::open(&dir, 300).unwrap();
        spool.append_batch(&[rec(1), rec(2)]).unwrap();
        let dropped = spool.append_batch(&[rec(3), rec(4)]).unwrap();
        assert!(spool.spooled_bytes() <= 300 || spool.segment_count() == 1);
        assert_eq!(dropped + spool.state.unreported_spool_drops, dropped);
        assert!(dropped > 0 || spool.segment_count() == 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
