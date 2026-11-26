//! `<data_dir>\model-storage.json`: start history (for LRU and the
//! N-starts-in-D-days promotion rule), cold archive records, and recent
//! move history. Not configuration: it changes on every start and during
//! every move, and must not churn `config.json` or trip activation
//! drift. Loading is tolerant — a missing or corrupt file starts empty.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use manager_config::path_key;

/// File name under the data dir.
pub const STATE_FILE: &str = "model-storage.json";
/// Current schema version.
pub const STATE_VERSION: u32 = 1;
/// Moves kept in [`StorageState::moves`].
pub const MOVE_HISTORY_CAP: usize = 50;
/// Start timestamps kept per file (beyond any sane N).
const STARTS_CAP: usize = 64;

/// Start history for one logical model file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub logical: PathBuf,
    /// Recent start times, oldest first (pruned to the promotion window).
    #[serde(default)]
    pub starts: Vec<DateTime<Utc>>,
    #[serde(default)]
    pub last_started: Option<DateTime<Utc>>,
    /// When the feature first saw this file, so idle demotion's clock
    /// starts at install rather than at the epoch.
    #[serde(default)]
    pub first_seen: Option<DateTime<Utc>>,
}

impl UsageRecord {
    /// The LRU key: the last start, else when first seen.
    pub fn last_used(&self) -> Option<DateTime<Utc>> {
        match (self.last_started, self.first_seen) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// A cold copy known to be intact: verified at `verified_at` with this
/// size and modification time. Demotion of the hot copy is an instant
/// delete while the cold file still matches the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRecord {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ms: i64,
    pub sha256: String,
    pub verified_at: DateTime<Utc>,
}

/// How a move ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoveOutcome {
    Running,
    Done,
    Failed,
    Cancelled,
    /// The app stopped mid-move (crash, power loss, exit).
    Interrupted,
}

/// One promotion or demotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoveRecord {
    pub id: String,
    pub logical: PathBuf,
    /// `"promote"` / `"demote"`.
    pub kind: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: MoveOutcome,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub bytes: u64,
    /// True when the demotion only deleted the hot copy (archive intact).
    #[serde(default)]
    pub instant: bool,
}

/// The whole state file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageState {
    pub version: u32,
    /// Keyed by [`path_key`] of the logical path.
    #[serde(default)]
    pub usage: BTreeMap<String, UsageRecord>,
    /// Keyed by [`path_key`] of the cold file path.
    #[serde(default)]
    pub archives: BTreeMap<String, ArchiveRecord>,
    /// Newest last, capped at [`MOVE_HISTORY_CAP`].
    #[serde(default)]
    pub moves: Vec<MoveRecord>,
}

impl Default for StorageState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            usage: BTreeMap::new(),
            archives: BTreeMap::new(),
            moves: Vec::new(),
        }
    }
}

/// Modification time of a file in ms since the epoch (None if unreadable).
pub fn file_mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(d.as_millis()).ok()
}

impl StorageState {
    /// Load from `path`; a missing or unparseable file yields an empty
    /// state (the second element carries the parse error, if any, for
    /// the caller to log).
    pub fn load(path: &Path) -> (StorageState, Option<String>) {
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (StorageState::default(), None),
            Err(e) => (StorageState::default(), Some(e.to_string())),
            Ok(bytes) => match serde_json::from_slice::<StorageState>(&bytes) {
                Ok(s) => (s, None),
                Err(e) => (StorageState::default(), Some(e.to_string())),
            },
        }
    }

    /// Write atomically: temp file + fsync + rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    fn usage_mut(&mut self, logical: &Path) -> &mut UsageRecord {
        self.usage
            .entry(path_key(logical))
            .or_insert_with(|| UsageRecord {
                logical: logical.to_path_buf(),
                ..Default::default()
            })
    }

    pub fn usage(&self, logical: &Path) -> Option<&UsageRecord> {
        self.usage.get(&path_key(logical))
    }

    /// Record an app-mediated start, pruning starts older than the window.
    pub fn record_start(&mut self, logical: &Path, now: DateTime<Utc>, window_days: u32) {
        let u = self.usage_mut(logical);
        u.starts.push(now);
        u.last_started = Some(now);
        let cutoff = now - Duration::days(i64::from(window_days.max(1)));
        u.starts.retain(|t| *t >= cutoff);
        if u.starts.len() > STARTS_CAP {
            let excess = u.starts.len() - STARTS_CAP;
            u.starts.drain(..excess);
        }
    }

    /// Stamp `first_seen` for a file with no history. Returns true when
    /// something changed.
    pub fn touch_first_seen(&mut self, logical: &Path, now: DateTime<Utc>) -> bool {
        let u = self.usage_mut(logical);
        if u.first_seen.is_none() {
            u.first_seen = Some(now);
            true
        } else {
            false
        }
    }

    /// Starts within the window ending at `now`.
    pub fn recent_starts(&self, logical: &Path, now: DateTime<Utc>, window_days: u32) -> u32 {
        let cutoff = now - Duration::days(i64::from(window_days.max(1)));
        self.usage(logical)
            .map(|u| u.starts.iter().filter(|t| **t >= cutoff).count() as u32)
            .unwrap_or(0)
    }

    /// Record a verified cold copy.
    pub fn set_archive(&mut self, rec: ArchiveRecord) {
        self.archives.insert(path_key(&rec.path), rec);
    }

    pub fn remove_archive(&mut self, cold_path: &Path) {
        self.archives.remove(&path_key(cold_path));
    }

    /// The archive record for `cold_path` when the file on disk still
    /// matches it (same size and mtime) and — when `pin` is a real pin —
    /// the recorded hash equals the pin.
    pub fn valid_archive(&self, cold_path: &Path, pin: Option<&str>) -> Option<&ArchiveRecord> {
        let rec = self.archives.get(&path_key(cold_path))?;
        let meta = std::fs::metadata(cold_path).ok().filter(|m| m.is_file())?;
        if meta.len() != rec.size || file_mtime_ms(&meta) != Some(rec.mtime_ms) {
            return None;
        }
        if let Some(pin) = pin {
            if !rec.sha256.eq_ignore_ascii_case(pin.trim()) {
                return None;
            }
        }
        Some(rec)
    }

    /// Append a move, keeping the history bounded.
    pub fn push_move(&mut self, m: MoveRecord) {
        self.moves.push(m);
        if self.moves.len() > MOVE_HISTORY_CAP {
            let excess = self.moves.len() - MOVE_HISTORY_CAP;
            self.moves.drain(..excess);
        }
    }

    /// Update the move with `id` in place.
    pub fn update_move(&mut self, id: &str, f: impl FnOnce(&mut MoveRecord)) {
        if let Some(m) = self.moves.iter_mut().find(|m| m.id == id) {
            f(m);
        }
    }

    /// Crash recovery: a move still `Running` in a freshly loaded state
    /// file was cut short. Returns how many were marked.
    pub fn mark_interrupted(&mut self, now: DateTime<Utc>) -> usize {
        let mut n = 0;
        for m in self
            .moves
            .iter_mut()
            .filter(|m| m.outcome == MoveOutcome::Running)
        {
            m.outcome = MoveOutcome::Interrupted;
            m.finished_at = Some(now);
            m.error = Some("the app stopped during the move; the source was kept".to_string());
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(days: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-26T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::days(days)
    }

    #[test]
    fn starts_are_pruned_to_the_window_and_counted() {
        let mut s = StorageState::default();
        let m = Path::new("models/X.gguf");
        s.record_start(m, t(0), 14);
        s.record_start(m, t(10), 14);
        assert_eq!(s.recent_starts(m, t(10), 14), 2);
        s.record_start(m, t(20), 14);
        assert_eq!(s.recent_starts(m, t(20), 14), 2, "day 0 fell out");
        assert_eq!(s.usage(m).unwrap().starts.len(), 2);
        assert_eq!(s.usage(m).unwrap().last_started, Some(t(20)));
        // Separator/case-insensitive key.
        assert_eq!(s.recent_starts(Path::new(r"MODELS\x.gguf"), t(20), 14), 2);
    }

    #[test]
    fn first_seen_is_stamped_once_and_feeds_last_used() {
        let mut s = StorageState::default();
        let m = Path::new("models/X.gguf");
        assert!(s.touch_first_seen(m, t(0)));
        assert!(!s.touch_first_seen(m, t(5)));
        assert_eq!(s.usage(m).unwrap().last_used(), Some(t(0)));
        s.record_start(m, t(3), 14);
        assert_eq!(s.usage(m).unwrap().last_used(), Some(t(3)));
    }

    #[test]
    fn save_load_round_trip_and_tolerant_load() {
        let dir = std::env::temp_dir().join(format!("llmdil-tier-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join(STATE_FILE);
        let (empty, err) = StorageState::load(&p);
        assert_eq!(empty, StorageState::default());
        assert!(err.is_none());
        let mut s = StorageState::default();
        s.record_start(Path::new("models/X.gguf"), t(0), 14);
        s.save(&p).unwrap();
        assert_eq!(StorageState::load(&p).0, s);
        std::fs::write(&p, b"{not json").unwrap();
        let (fallback, err) = StorageState::load(&p);
        assert_eq!(fallback, StorageState::default());
        assert!(err.is_some());
    }

    #[test]
    fn archive_is_valid_only_while_the_file_matches() {
        let dir = std::env::temp_dir().join(format!("llmdil-tier-arch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("X.gguf");
        std::fs::write(&f, b"archived").unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let mut s = StorageState::default();
        s.set_archive(ArchiveRecord {
            path: f.clone(),
            size: meta.len(),
            mtime_ms: file_mtime_ms(&meta).unwrap(),
            sha256: "ab".repeat(32),
            verified_at: t(0),
        });
        assert!(s.valid_archive(&f, None).is_some());
        assert!(s.valid_archive(&f, Some(&"AB".repeat(32))).is_some());
        assert!(
            s.valid_archive(&f, Some(&"cd".repeat(32))).is_none(),
            "re-pinned"
        );
        std::fs::write(&f, b"archived but changed").unwrap();
        assert!(s.valid_archive(&f, None).is_none(), "size changed");
        std::fs::remove_file(&f).unwrap();
        assert!(s.valid_archive(&f, None).is_none(), "gone");
    }

    #[test]
    fn running_moves_are_marked_interrupted_and_history_is_capped() {
        let mut s = StorageState::default();
        for i in 0..(MOVE_HISTORY_CAP + 5) {
            s.push_move(MoveRecord {
                id: format!("mv-{i}"),
                logical: PathBuf::from("models/X.gguf"),
                kind: "promote".into(),
                from: "cold".into(),
                to: "hot".into(),
                reason: "manual".into(),
                started_at: t(0),
                finished_at: None,
                outcome: if i == MOVE_HISTORY_CAP + 4 {
                    MoveOutcome::Running
                } else {
                    MoveOutcome::Done
                },
                error: None,
                bytes: 0,
                instant: false,
            });
        }
        assert_eq!(s.moves.len(), MOVE_HISTORY_CAP);
        assert_eq!(s.mark_interrupted(t(1)), 1);
        assert_eq!(s.moves.last().unwrap().outcome, MoveOutcome::Interrupted);
    }
}
