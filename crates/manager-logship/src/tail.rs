//! File tail for the app's own append-only log files (design
//! §6.2). Offset state is owned by the caller (persisted in the
//! spool's `state.json`), so a restart resumes where it left off
//! instead of re-shipping history. Rotation — the writers rename
//! the current file to its `.prev` sibling past ~1 MiB — is detected
//! by file identity or size regression: the remainder is drained
//! from `.prev`, then the new file is read from the start. Where
//! no usable file identity exists, a fingerprint of the consumed
//! region stands in: replaced bytes before the offset give the
//! rotation away.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// How much history a first-ever sight of a file ships: the last
/// 64 KiB, aligned to a line boundary — recent context for the
/// debugging session that just enabled streaming, not the whole
/// file.
const FIRST_SIGHT_BYTES: u64 = 64 * 1024;

/// A platform file identity, used to detect rotation even when
/// the new file is the same size as — or larger than — the saved
/// offset into the old one (design §6.2: rotation is detected by
/// file identity/size regression). Unix exposes device + inode;
/// Windows std exposes neither a stable file index nor a volume
/// serial (`windows_by_handle` is unstable), so the file's
/// creation time stands in — a rotated-in file is by construction
/// newly created. `None` where no identity is available; rotation
/// detection then falls back to the size-regression rule alone.
#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.dev() ^ meta.ino().rotate_left(32))
}

#[cfg(windows)]
fn file_identity(meta: &fs::Metadata) -> Option<u64> {
    let created = meta.created().ok()?;
    let nanos = created
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((nanos as u64) ^ ((nanos >> 64) as u64))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(meta: &fs::Metadata) -> Option<u64> {
    let _ = meta;
    None
}

pub struct FileTail {
    /// Key this tail's offset and fingerprint persist under in the
    /// spool state. Equal to `source` for single-file sources;
    /// distinct per file where one source has several (`gateway`
    /// stdout/stderr share a source and differ by stream).
    pub key: String,
    pub source: String,
    /// The wire record's `stream` for lines from this file.
    pub stream: String,
    /// Whether the file existed at the last poll (per-source status).
    pub present: bool,
    path: PathBuf,
    prev_path: PathBuf,
    /// Byte offset into the current file; `None` until first poll.
    pub offset: Option<u64>,
    /// Identity of the file `offset` belongs to, for rotation
    /// detection. In-memory only: after a restart the first poll
    /// re-baselines it (a rotation during downtime is still caught
    /// by the size-regression rule when the new file is smaller).
    file_id: Option<u64>,
    /// Fingerprint of the last bytes consumed (the region ending
    /// at `offset`). Append-only writers never change bytes before
    /// the offset, so a mismatch means the file was replaced —
    /// the rotation signal that works everywhere, including
    /// Windows, where creation timestamps of the old and new file
    /// can fall in the same tick and compare equal. Persisted by
    /// the caller beside the offset (see [`Self::fingerprint`]) so
    /// a file replaced while the shipper was down — by one at least
    /// as long as the saved offset — is still recognised.
    consumed_fingerprint: Option<u64>,
}

/// How many bytes back from the offset the fingerprint covers:
/// enough to make an accidental match across a rotation
/// implausible, cheap to re-read on every poll.
const FINGERPRINT_WINDOW: u64 = 4096;

/// FNV-1a over the last [`FINGERPRINT_WINDOW`] bytes ending at
/// `offset` (fewer near the start of the file; empty at 0).
fn window_fingerprint(path: &PathBuf, offset: u64) -> Option<u64> {
    let start = offset.saturating_sub(FINGERPRINT_WINDOW);
    let mut f = fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; (offset - start) as usize];
    f.read_exact(&mut buf).ok()?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in buf {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(h)
}

impl FileTail {
    pub fn new(source: &str, path: PathBuf, prev_path: PathBuf, offset: Option<u64>) -> Self {
        Self {
            key: source.to_string(),
            source: source.to_string(),
            stream: "stdout".to_string(),
            present: false,
            path,
            prev_path,
            offset,
            file_id: None,
            consumed_fingerprint: None,
        }
    }

    /// Set the state key and wire stream for a source that tails
    /// more than one file.
    pub fn with_stream(mut self, key: &str, stream: &str) -> Self {
        self.key = key.to_string();
        self.stream = stream.to_string();
        self
    }

    /// Restore the persisted fingerprint of the consumed region
    /// (the value [`Self::fingerprint`] returned before a restart).
    pub fn with_fingerprint(mut self, fingerprint: Option<u64>) -> Self {
        self.consumed_fingerprint = fingerprint;
        self
    }

    /// Fingerprint of the region ending at `offset`, for the caller
    /// to persist with the offset.
    pub fn fingerprint(&self) -> Option<u64> {
        self.consumed_fingerprint
    }

    /// Read every complete line appended since the last poll.
    /// Incomplete trailing lines (no newline yet) wait for their
    /// remainder.
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(meta) = fs::metadata(&self.path) else {
            self.present = false;
            return out;
        };
        self.present = true;
        let len = meta.len();
        let id = file_identity(&meta);
        let mut offset = match self.offset {
            None => first_sight_offset(&self.path, len),
            Some(o) => o,
        };
        // Rotation (or truncation): the file shrank below our
        // offset, or it is a different file than the one the
        // offset belongs to — a rename + recreate can leave a
        // same-size or larger file, which size alone cannot see.
        // The consumed-region fingerprint is the check of last
        // resort: it needs no platform identity at all.
        let fingerprint_mismatch = match self.offset {
            Some(o) if o > 0 && len >= o => matches!(
                (self.consumed_fingerprint, window_fingerprint(&self.path, o)),
                (Some(stored), Some(current)) if stored != current
            ),
            _ => false,
        };
        let rotated = len < offset
            || matches!((self.file_id, id), (Some(prev), Some(cur)) if prev != cur)
            || fingerprint_mismatch;
        if rotated {
            // Drain the unread remainder from the .prev sibling,
            // then start the new file at 0.
            if let Ok(prev_meta) = fs::metadata(&self.prev_path) {
                if prev_meta.len() > offset {
                    out.extend(read_lines(&self.prev_path, offset, prev_meta.len()));
                }
            }
            offset = 0;
        }
        if len > offset {
            let lines = read_lines(&self.path, offset, len);
            // Advance only past complete lines.
            offset += consumed_bytes(&self.path, offset, len);
            out.extend(lines);
        }
        self.offset = Some(offset);
        if id.is_some() {
            self.file_id = id;
        }
        // Re-baseline the fingerprint on whatever file and offset
        // this poll settled on.
        self.consumed_fingerprint = window_fingerprint(&self.path, offset);
        out
    }
}

fn first_sight_offset(path: &PathBuf, len: u64) -> u64 {
    if len <= FIRST_SIGHT_BYTES {
        return 0;
    }
    let start = len - FIRST_SIGHT_BYTES;
    // Align to just past the next newline so the first shipped line
    // is whole.
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return len,
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return len;
    }
    let mut buf = Vec::new();
    if f.take(FIRST_SIGHT_BYTES).read_to_end(&mut buf).is_err() {
        return len;
    }
    match buf.iter().position(|&b| b == b'\n') {
        Some(pos) => start + pos as u64 + 1,
        None => len,
    }
}

/// Bytes up to and including the last newline in `[offset, len)`.
fn consumed_bytes(path: &PathBuf, offset: u64, len: u64) -> u64 {
    let Ok(mut f) = fs::File::open(path) else {
        return 0;
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return 0;
    }
    let mut buf = vec![0u8; (len - offset) as usize];
    if f.read_exact(&mut buf).is_err() {
        return 0;
    }
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(pos) => pos as u64 + 1,
        None => 0,
    }
}

fn read_lines(path: &PathBuf, offset: u64, len: u64) -> Vec<String> {
    let Ok(mut f) = fs::File::open(path) else {
        return Vec::new();
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }
    let mut buf = vec![0u8; (len - offset) as usize];
    if f.read_exact(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = Vec::new();
    // Only complete lines: everything up to the last newline.
    if let Some(last) = text.rfind('\n') {
        for line in text[..last].split('\n') {
            lines.push(line.strip_suffix('\r').unwrap_or(line).to_string());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("logship-tail-{name}-{}", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn tail_reads_appends_and_waits_for_complete_lines() {
        let path = temp_file("basic.log");
        let prev = temp_file("basic.log.prev");
        fs::write(&path, "one\ntwo\n").unwrap();
        let mut tail = FileTail::new("wizard", path.clone(), prev.clone(), None);
        assert_eq!(tail.poll(), vec!["one".to_string(), "two".to_string()]);
        assert!(tail.poll().is_empty());
        // Partial line waits.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"thr").unwrap();
        drop(f);
        assert!(tail.poll().is_empty());
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"ee\n").unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["three".to_string()]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn tail_drains_prev_on_rotation() {
        let path = temp_file("rot.log");
        let prev = temp_file("rot.log.prev");
        fs::write(&path, "a\nb\nc\n").unwrap();
        let mut tail = FileTail::new("wizard", path.clone(), prev.clone(), None);
        assert_eq!(tail.poll().len(), 3);
        // More lines, then rotation: file -> prev, fresh file.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"d\ne\n").unwrap();
        drop(f);
        fs::rename(&path, &prev).unwrap();
        fs::write(&path, "fresh\n").unwrap();
        assert_eq!(
            tail.poll(),
            vec!["d".to_string(), "e".to_string(), "fresh".to_string()]
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&prev);
    }

    /// Sources without a `.prev` convention (service-manage, audit,
    /// service-mode backend logs: append-only, never rotated by
    /// their writers) must still survive an operator truncating or
    /// replacing the file: nothing re-shipped, nothing lost.
    #[test]
    fn tail_survives_truncation_without_prev() {
        let path = temp_file("trunc.log");
        let prev = temp_file("trunc.log.prev");
        fs::write(&path, "a\nb\nc\n").unwrap();
        let mut tail = FileTail::new("audit", path.clone(), prev.clone(), None);
        assert_eq!(tail.poll().len(), 3);
        // Truncated in place, then appended to.
        fs::write(&path, "x\n").unwrap();
        assert_eq!(tail.poll(), vec!["x".to_string()]);
        assert!(tail.poll().is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn tail_survives_larger_replacement_without_prev() {
        let path = temp_file("repl.log");
        let prev = temp_file("repl.log.prev");
        fs::write(&path, "one\ntwo\n").unwrap();
        let mut tail = FileTail::new("service-manage", path.clone(), prev.clone(), None);
        assert_eq!(tail.poll().len(), 2);
        // Replaced by a longer file whose prefix differs: size alone
        // cannot see it; the consumed-region fingerprint does.
        fs::remove_file(&path).unwrap();
        fs::write(&path, "uno\ndos\ntres\n").unwrap();
        assert_eq!(
            tail.poll(),
            vec!["uno".to_string(), "dos".to_string(), "tres".to_string()]
        );
        assert!(tail.poll().is_empty());
        let _ = fs::remove_file(&path);
    }

    /// A restart resumes from the persisted offset: no history is
    /// re-shipped, and lines appended while down are picked up.
    #[test]
    fn tail_resumes_from_persisted_offset_and_fingerprint() {
        let path = temp_file("resume.log");
        let prev = temp_file("resume.log.prev");
        fs::write(&path, "a\nb\n").unwrap();
        let mut tail = FileTail::new("gateway", path.clone(), prev.clone(), None);
        assert_eq!(tail.poll().len(), 2);
        let (offset, fp) = (tail.offset, tail.fingerprint());
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"c\n").unwrap();
        drop(f);
        let mut resumed =
            FileTail::new("gateway", path.clone(), prev.clone(), offset).with_fingerprint(fp);
        assert_eq!(resumed.poll(), vec!["c".to_string()]);
        let _ = fs::remove_file(&path);
    }

    /// The gateway logs rotate to `.prev` on every gateway spawn. A
    /// rotation while the shipper was down, to a new file already
    /// longer than the saved offset, is caught by the persisted
    /// fingerprint: the old run's unread tail drains from `.prev`
    /// and the new run ships from its first line.
    #[test]
    fn tail_detects_rotation_during_downtime_via_persisted_fingerprint() {
        let path = temp_file("down.log");
        let prev = temp_file("down.log.prev");
        fs::write(&path, "old1\nold2\n").unwrap();
        let mut tail = FileTail::new("gateway", path.clone(), prev.clone(), None);
        assert_eq!(tail.poll().len(), 2);
        let (offset, fp) = (tail.offset, tail.fingerprint());
        drop(tail);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"old3\n").unwrap();
        drop(f);
        fs::rename(&path, &prev).unwrap();
        fs::write(&path, "new1\nnew2\nnew3\nnew4\n").unwrap();
        let mut resumed =
            FileTail::new("gateway", path.clone(), prev.clone(), offset).with_fingerprint(fp);
        assert_eq!(
            resumed.poll(),
            vec![
                "old3".to_string(),
                "new1".to_string(),
                "new2".to_string(),
                "new3".to_string(),
                "new4".to_string()
            ]
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&prev);
    }

    #[test]
    fn tail_reports_presence() {
        let path = temp_file("presence.log");
        let prev = temp_file("presence.log.prev");
        let mut tail = FileTail::new("audit", path.clone(), prev, None);
        assert!(tail.poll().is_empty());
        assert!(!tail.present);
        assert_eq!(tail.offset, None);
        fs::write(&path, "x\n").unwrap();
        assert_eq!(tail.poll().len(), 1);
        assert!(tail.present);
        let _ = fs::remove_file(&path);
    }
}
