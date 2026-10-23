//! File tail for the app's own append-only log files (design
//! §6.2). Offset state is owned by the caller (persisted in the
//! spool's `state.json`), so a restart resumes where it left off
//! instead of re-shipping history. Rotation — the writers rename
//! the current file to its `.prev` sibling past ~1 MiB — is detected
//! by size regression: the remainder is drained from `.prev`, then
//! the new file is read from the start.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// How much history a first-ever sight of a file ships: the last
/// 64 KiB, aligned to a line boundary — recent context for the
/// debugging session that just enabled streaming, not the whole
/// file.
const FIRST_SIGHT_BYTES: u64 = 64 * 1024;

pub struct FileTail {
    pub source: String,
    path: PathBuf,
    prev_path: PathBuf,
    /// Byte offset into the current file; `None` until first poll.
    pub offset: Option<u64>,
}

impl FileTail {
    pub fn new(source: &str, path: PathBuf, prev_path: PathBuf, offset: Option<u64>) -> Self {
        Self {
            source: source.to_string(),
            path,
            prev_path,
            offset,
        }
    }

    /// Read every complete line appended since the last poll.
    /// Incomplete trailing lines (no newline yet) wait for their
    /// remainder.
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(meta) = fs::metadata(&self.path) else {
            return out;
        };
        let len = meta.len();
        let mut offset = match self.offset {
            None => first_sight_offset(&self.path, len),
            Some(o) => o,
        };
        if len < offset {
            // Rotation (or truncation): drain the unread remainder
            // from the .prev sibling, then start the new file at 0.
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
}
