//! The crash-safe, verified tier copy.
//!
//! `copy_verified(src, dest)` writes `dest.partial`, hashing the source
//! stream as it reads; flushes and fsyncs; re-reads the partial file and
//! hashes it; and renames it over `dest` only when both hashes equal the
//! expected one (the model's SHA-256 pin, or the source stream's own
//! hash when there is no pin). The source is never modified. A crash at
//! any point leaves at most a `.partial` file, which never resolves and
//! is swept at the next start ([`sweep_partials`]).
//!
//! The copying thread runs in Windows background mode (very-low I/O
//! priority and low memory priority, so copied pages are the first out
//! of the standby list rather than evicting cached pages of hot models),
//! is throttled, and parks while [`CopyControl::pause`] is set — the app
//! sets it whenever a backend is loading.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::TierError;
use manager_config::{tier_partial_path, TIER_PARTIAL_SUFFIX};

const BUF_BYTES: usize = 8 * 1024 * 1024;
const PAUSE_POLL: Duration = Duration::from_millis(200);

/// Which pass a copy is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyPhase {
    Copying = 0,
    Verifying = 1,
    Done = 2,
}

/// Shared control and progress for one copy. The app holds an `Arc` of
/// it: it sets `cancel` / `pause` and reads the counters for progress.
#[derive(Debug, Default)]
pub struct CopyControl {
    pub cancel: AtomicBool,
    pub pause: AtomicBool,
    /// Bytes processed in the current phase.
    pub done_bytes: AtomicU64,
    /// File size (each phase processes this many bytes).
    pub total_bytes: AtomicU64,
    phase: AtomicU8,
}

impl CopyControl {
    pub fn phase(&self) -> CopyPhase {
        match self.phase.load(Ordering::SeqCst) {
            0 => CopyPhase::Copying,
            1 => CopyPhase::Verifying,
            _ => CopyPhase::Done,
        }
    }

    /// Overall progress in [0, 1]: copying is the first half, verifying
    /// the second.
    pub fn fraction(&self) -> f64 {
        let total = self.total_bytes.load(Ordering::SeqCst);
        if total == 0 {
            return 0.0;
        }
        let part = self.done_bytes.load(Ordering::SeqCst) as f64 / total as f64;
        match self.phase() {
            CopyPhase::Copying => part * 0.5,
            CopyPhase::Verifying => 0.5 + part * 0.5,
            CopyPhase::Done => 1.0,
        }
    }

    fn set_phase(&self, p: CopyPhase) {
        self.phase.store(p as u8, Ordering::SeqCst);
        self.done_bytes.store(0, Ordering::SeqCst);
    }
}

/// Copy tuning.
#[derive(Debug, Clone, Copy)]
pub struct CopyOptions {
    /// Throttle in bytes/s for each pass; 0 = unthrottled.
    pub rate_bytes_per_sec: u64,
    /// Run the copying thread in Windows background mode.
    pub background_io: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            rate_bytes_per_sec: 0,
            background_io: true,
        }
    }
}

/// A finished copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOutcome {
    /// SHA-256 (lowercase hex) of the bytes now at `dest`.
    pub sha256: String,
    pub bytes: u64,
}

/// Copy `src` to `dest` crash-safely and verified (see the module docs).
/// `expected` is the model's real SHA-256 pin when it has one. Blocking:
/// run it on a dedicated thread. An existing `dest` is replaced only by a
/// verified copy.
pub fn copy_verified(
    src: &Path,
    dest: &Path,
    expected: Option<&str>,
    opts: CopyOptions,
    ctl: &CopyControl,
) -> Result<CopyOutcome, TierError> {
    let _bg = if opts.background_io {
        Some(BackgroundMode::enter())
    } else {
        None
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(TierError::from_io)?;
    }
    let partial = tier_partial_path(dest);
    let _ = std::fs::remove_file(&partial);
    let result = copy_to_partial(src, &partial, dest, expected, opts, ctl);
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

fn copy_to_partial(
    src: &Path,
    partial: &Path,
    dest: &Path,
    expected: Option<&str>,
    opts: CopyOptions,
    ctl: &CopyControl,
) -> Result<CopyOutcome, TierError> {
    let expected = expected.map(|e| e.trim().to_ascii_lowercase());
    let mut reader = std::fs::File::open(src).map_err(TierError::from_io)?;
    let len = reader.metadata().map_err(TierError::from_io)?.len();
    ctl.total_bytes.store(len, Ordering::SeqCst);

    ctl.set_phase(CopyPhase::Copying);
    let source_hash = {
        let mut writer = std::fs::File::create(partial).map_err(TierError::from_io)?;
        let h = stream(&mut reader, Some(&mut writer), opts, ctl)?;
        writer.flush().map_err(TierError::from_io)?;
        writer.sync_all().map_err(TierError::from_io)?;
        h
    };
    drop(reader);
    if let Some(exp) = &expected {
        if &source_hash != exp {
            return Err(TierError::SourceMismatch {
                expected: exp.clone(),
                actual: source_hash,
            });
        }
    }
    let want = expected.unwrap_or_else(|| source_hash.clone());

    let copy_hash = verify_copy(partial, &want, opts, ctl)?;
    std::fs::rename(partial, dest).map_err(TierError::from_io)?;
    ctl.set_phase(CopyPhase::Done);
    Ok(CopyOutcome {
        sha256: copy_hash,
        bytes: len,
    })
}

/// The verifying pass: re-read the written copy and require `want`.
fn verify_copy(
    partial: &Path,
    want: &str,
    opts: CopyOptions,
    ctl: &CopyControl,
) -> Result<String, TierError> {
    ctl.set_phase(CopyPhase::Verifying);
    let mut check = std::fs::File::open(partial).map_err(TierError::from_io)?;
    let copy_hash = stream(&mut check, None, opts, ctl)?;
    if copy_hash != want {
        return Err(TierError::DestMismatch {
            expected: want.to_string(),
            actual: copy_hash,
        });
    }
    Ok(copy_hash)
}

/// Read `reader` to the end, hashing (and writing to `writer` when
/// given), honoring cancel / pause / throttle. Returns the hex digest.
pub(crate) fn stream(
    reader: &mut dyn Read,
    mut writer: Option<&mut dyn Write>,
    opts: CopyOptions,
    ctl: &CopyControl,
) -> Result<String, TierError> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF_BYTES];
    // Throttle window: restarted after every pause so paused time does
    // not bank a burst.
    let mut window_start = Instant::now();
    let mut window_bytes: u64 = 0;
    loop {
        if ctl.cancel.load(Ordering::SeqCst) {
            return Err(TierError::Cancelled);
        }
        if ctl.pause.load(Ordering::SeqCst) {
            while ctl.pause.load(Ordering::SeqCst) {
                if ctl.cancel.load(Ordering::SeqCst) {
                    return Err(TierError::Cancelled);
                }
                std::thread::sleep(PAUSE_POLL);
            }
            window_start = Instant::now();
            window_bytes = 0;
        }
        let n = reader.read(&mut buf).map_err(TierError::from_io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if let Some(w) = writer.as_deref_mut() {
            w.write_all(&buf[..n]).map_err(TierError::from_io)?;
        }
        ctl.done_bytes.fetch_add(n as u64, Ordering::SeqCst);
        window_bytes += n as u64;
        if opts.rate_bytes_per_sec > 0 {
            let due = Duration::from_secs_f64(window_bytes as f64 / opts.rate_bytes_per_sec as f64);
            let elapsed = window_start.elapsed();
            if due > elapsed {
                std::thread::sleep(due - elapsed);
            }
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Delete every leftover `*.partial` copy directly in `dir` (crash
/// recovery at start). Returns the paths removed. A missing directory
/// is not an error.
pub fn sweep_partials(dir: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return removed;
    };
    for e in entries.flatten() {
        let p = e.path();
        let is_partial = p
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(TIER_PARTIAL_SUFFIX))
            .unwrap_or(false);
        if is_partial && p.is_file() && std::fs::remove_file(&p).is_ok() {
            removed.push(p);
        }
    }
    removed
}

/// RAII guard for Windows thread background mode.
struct BackgroundMode {
    #[cfg(windows)]
    entered: bool,
}

impl BackgroundMode {
    fn enter() -> Self {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            };
            // SAFETY: GetCurrentThread returns a pseudo-handle valid for
            // the calling thread; SetThreadPriority has no memory effects.
            let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
            BackgroundMode { entered: ok != 0 }
        }
        #[cfg(not(windows))]
        {
            BackgroundMode {}
        }
    }
}

impl Drop for BackgroundMode {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.entered {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
            };
            // SAFETY: as in `enter`.
            unsafe {
                SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "llmdil-tier-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn opts() -> CopyOptions {
        CopyOptions {
            rate_bytes_per_sec: 0,
            background_io: true,
        }
    }

    #[test]
    fn copies_and_verifies_against_the_pin() {
        let d = tmpdir("pin");
        let src = d.join("src").join("m.gguf");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        let data: Vec<u8> = (0..(3 * BUF_BYTES + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dest = d.join("dst").join("m.gguf");
        let ctl = CopyControl::default();
        let out =
            copy_verified(&src, &dest, Some(&sha(&data).to_uppercase()), opts(), &ctl).unwrap();
        assert_eq!(out.sha256, sha(&data));
        assert_eq!(out.bytes, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(src.exists(), "the source is never touched");
        assert!(!tier_partial_path(&dest).exists());
        assert_eq!(ctl.phase(), CopyPhase::Done);
        assert_eq!(ctl.fraction(), 1.0);
    }

    #[test]
    fn no_pin_verifies_against_the_source_stream() {
        let d = tmpdir("nopin");
        let src = d.join("a.gguf");
        std::fs::write(&src, b"hello tiers").unwrap();
        let dest = d.join("b").join("a.gguf");
        let out = copy_verified(&src, &dest, None, opts(), &CopyControl::default()).unwrap();
        assert_eq!(out.sha256, sha(b"hello tiers"));
    }

    #[test]
    fn a_source_that_does_not_match_its_pin_is_refused_and_leaves_nothing() {
        let d = tmpdir("badsrc");
        let src = d.join("a.gguf");
        std::fs::write(&src, b"corrupted").unwrap();
        let dest = d.join("b").join("a.gguf");
        let err = copy_verified(
            &src,
            &dest,
            Some(&sha(b"original")),
            opts(),
            &CopyControl::default(),
        )
        .unwrap_err();
        assert!(matches!(err, TierError::SourceMismatch { .. }), "{err:?}");
        assert!(!dest.exists());
        assert!(!tier_partial_path(&dest).exists());
        assert!(src.exists());
    }

    #[test]
    fn an_existing_destination_is_replaced_only_by_a_verified_copy() {
        let d = tmpdir("replace");
        let src = d.join("a.gguf");
        std::fs::write(&src, b"new bytes").unwrap();
        let dest = d.join("c").join("a.gguf");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"stale").unwrap();
        // Failing copy: the stale destination survives.
        let _ = copy_verified(
            &src,
            &dest,
            Some(&sha(b"other")),
            opts(),
            &CopyControl::default(),
        )
        .unwrap_err();
        assert_eq!(std::fs::read(&dest).unwrap(), b"stale");
        // Verified copy replaces it.
        copy_verified(
            &src,
            &dest,
            Some(&sha(b"new bytes")),
            opts(),
            &CopyControl::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new bytes");
    }

    #[test]
    fn cancel_removes_the_partial_copy() {
        let d = tmpdir("cancel");
        let src = d.join("a.gguf");
        std::fs::write(&src, vec![7u8; 1024]).unwrap();
        let dest = d.join("b").join("a.gguf");
        let ctl = CopyControl::default();
        ctl.cancel.store(true, Ordering::SeqCst);
        let err = copy_verified(&src, &dest, None, opts(), &ctl).unwrap_err();
        assert_eq!(err, TierError::Cancelled);
        assert!(!dest.exists());
        assert!(!tier_partial_path(&dest).exists());
    }

    #[test]
    fn pause_parks_the_copy_until_released() {
        let d = tmpdir("pause");
        let src = d.join("a.gguf");
        std::fs::write(&src, vec![1u8; 4096]).unwrap();
        let dest = d.join("b").join("a.gguf");
        let ctl = Arc::new(CopyControl::default());
        ctl.pause.store(true, Ordering::SeqCst);
        let worker = {
            let ctl = Arc::clone(&ctl);
            let (src, dest) = (src.clone(), dest.clone());
            std::thread::spawn(move || copy_verified(&src, &dest, None, opts(), &ctl))
        };
        std::thread::sleep(Duration::from_millis(500));
        assert!(!dest.exists(), "paused: nothing committed");
        assert_eq!(ctl.done_bytes.load(Ordering::SeqCst), 0);
        ctl.pause.store(false, Ordering::SeqCst);
        worker.join().unwrap().unwrap();
        assert!(dest.exists());
    }

    #[test]
    fn throttle_limits_the_rate() {
        let data = vec![0u8; 2 * 1024 * 1024];
        let ctl = CopyControl::default();
        let t = Instant::now();
        stream(
            &mut &data[..],
            None,
            CopyOptions {
                rate_bytes_per_sec: 4 * 1024 * 1024,
                background_io: false,
            },
            &ctl,
        )
        .unwrap();
        assert!(
            t.elapsed() >= Duration::from_millis(400),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn a_copy_that_does_not_read_back_as_written_is_a_destination_mismatch() {
        let d = tmpdir("destbad");
        let partial = d.join("a.gguf.partial");
        std::fs::write(&partial, b"flipped bit").unwrap();
        let err =
            verify_copy(&partial, &sha(b"original"), opts(), &CopyControl::default()).unwrap_err();
        assert!(matches!(err, TierError::DestMismatch { .. }), "{err:?}");
    }

    struct FullDisk;
    impl Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_destination_is_reported_as_disk_full() {
        let data = vec![0u8; 1024];
        let mut w = FullDisk;
        let err = stream(
            &mut &data[..],
            Some(&mut w),
            opts(),
            &CopyControl::default(),
        )
        .unwrap_err();
        assert_eq!(err, TierError::DiskFull);
        #[cfg(windows)]
        assert!(super::super::is_disk_full(
            &std::io::Error::from_raw_os_error(112)
        ));
    }

    #[test]
    fn sweep_removes_only_partials() {
        let d = tmpdir("sweep");
        std::fs::write(d.join("a.gguf"), b"x").unwrap();
        std::fs::write(d.join("a.gguf.partial"), b"x").unwrap();
        std::fs::write(d.join("b.gguf.part"), b"x").unwrap();
        let removed = sweep_partials(&d);
        assert_eq!(removed, vec![d.join("a.gguf.partial")]);
        assert!(d.join("a.gguf").exists());
        assert!(
            d.join("b.gguf.part").exists(),
            "download partials are not ours"
        );
        assert!(sweep_partials(&d.join("missing")).is_empty());
    }
}
