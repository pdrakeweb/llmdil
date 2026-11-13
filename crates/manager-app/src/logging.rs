//! The app's one logging pipeline (T-05).
//!
//! Before this module no `tracing` subscriber was installed anywhere,
//! so every `tracing::` event in the workspace was discarded and the
//! fallback `eprintln!` lines were invisible in the windowless
//! release build. [`init`] installs exactly one global subscriber
//! with two layers, each behind its own `RUST_LOG`-overridable
//! [`EnvFilter`] (default [`DEFAULT_FILTER`]):
//!
//! - a human-readable stderr layer (the dev console / `tauri dev`);
//! - a JSON Lines file layer at `<data_dir>/logs/app.log` — one JSON
//!   object per event, carrying the current span and the span list,
//!   so an event inside a task carries that task's `task_id` (see
//!   [`crate::tasks::op_span`]).
//!
//! `app.log` is size-bounded the way the app's other file logs are
//! (`wizard.log`, `optimize.log`, the gateway logs): past
//! [`APP_LOG_MAX_BYTES`] (~1 MiB) it rotates to its `.prev` sibling
//! (`app.log.prev`, via `prev_log_path`), so at most ~2 MiB is kept.
//! The diagnostics bundle carries both files as the `app` scope
//! ([`app_log_lines`]).
//!
//! Secrets: the layers write the fields call sites give them, nothing
//! more — no request bodies, no environment, no config dump. Call
//! sites must keep secret values out of event fields (the existing
//! ones log ids, paths, counts and error text only).
//!
//! ## Hook for log streaming (not wired here)
//!
//! The log-streaming shipper (`manager-logship`, owned elsewhere)
//! takes app records through its in-process `RecordTap`. This module
//! does not feed tracing events to it. A future change that wants
//! tracing events shipped can build a `Layer` that turns events into
//! `TapRecord`s and install it with [`init_with_layers`] — extra
//! layers join the same registry, after the stderr and file layers,
//! each with whatever per-layer filter it chooses. Such a layer must
//! not itself emit tracing events from inside `on_event` (that
//! re-enters the subscriber), and the shipper's own `tracing` output
//! should be filtered out of it to avoid a feedback loop.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use manager_wizard::ops::prev_log_path;

/// The JSON Lines app log's file name under `<data_dir>/logs`.
pub const APP_LOG_FILE: &str = "app.log";

/// Rotation threshold for `app.log` — the same ~1 MiB the wizard,
/// optimize and gateway logs rotate at.
pub const APP_LOG_MAX_BYTES: u64 = 1_048_576;

/// Filter used when `RUST_LOG` is unset or unparsable.
pub const DEFAULT_FILTER: &str = "info";

/// Most app-log lines a diagnostics bundle carries (across
/// `app.log.prev` and `app.log`), matching the per-scope cap the
/// bundle applies to supervisor rings.
pub const DIAGNOSTICS_MAX_LINES: usize = 5000;

/// A layer that can join the pipeline's registry (see the module
/// docs' log-streaming hook).
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// `<data_dir>/logs/app.log`.
pub fn app_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join(APP_LOG_FILE)
}

/// The filter each layer uses: `RUST_LOG` when set and valid, else
/// [`DEFAULT_FILTER`].
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Install the global subscriber (stderr + `app.log`). Idempotent:
/// the first call installs it, later calls do nothing. Returns
/// whether the pipeline is the process's global subscriber (false
/// when something else installed one first).
pub fn init(data_dir: &Path) -> bool {
    init_with_layers(data_dir, Vec::new())
}

/// [`init`] plus `extra` layers in the same registry — the hook a
/// log-streaming layer would use. `extra` is ignored once the
/// pipeline is installed.
pub fn init_with_layers(data_dir: &Path, extra: Vec<BoxedLayer>) -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let writer = RotatingFileWriter::new(app_log_path(data_dir), APP_LOG_MAX_BYTES);
        let mut layers = pipeline_layers(writer, true);
        layers.extend(extra);
        tracing_subscriber::registry()
            .with(layers)
            .try_init()
            .is_ok()
    })
}

/// The pipeline's layers: optionally stderr, always the JSON Lines
/// file layer on `writer`.
pub fn pipeline_layers(writer: RotatingFileWriter, stderr: bool) -> Vec<BoxedLayer> {
    let mut layers: Vec<BoxedLayer> = Vec::new();
    if stderr {
        // Under `cargo test` the "stderr" layer goes through the test
        // harness's capture instead, so a test that installs the
        // global pipeline does not spray every later test's events
        // over the test output.
        #[cfg(not(test))]
        let console = io::stderr;
        #[cfg(test)]
        let console = fmt::TestWriter::new();
        layers.push(
            fmt::layer()
                .with_writer(console)
                .with_target(true)
                .with_filter(env_filter())
                .boxed(),
        );
    }
    layers.push(
        fmt::layer()
            .json()
            .with_writer(writer)
            .with_current_span(true)
            .with_span_list(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_filter(env_filter())
            .boxed(),
    );
    layers
}

/// A subscriber with only the file layer on `writer` — what tests
/// install as a scoped default to inspect `app.log` output without
/// touching the process-global subscriber.
pub fn file_subscriber(writer: RotatingFileWriter) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::registry().with(pipeline_layers(writer, false))
}

/// Append-only, size-bounded log file: before a write that would take
/// the file past `max_bytes`, the file rotates to its `.prev`
/// sibling (replacing the previous one). Writing never fails the
/// caller — an unwritable log drops the line.
#[derive(Clone)]
pub struct RotatingFileWriter {
    inner: Arc<Mutex<RotState>>,
}

struct RotState {
    path: PathBuf,
    max_bytes: u64,
    file: Option<File>,
    len: u64,
}

impl RotState {
    fn open(&mut self) {
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        if let Ok(f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.len = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(f);
        }
    }

    fn rotate(&mut self) {
        // Close first: Windows refuses to rename an open file.
        self.file = None;
        let _ = std::fs::rename(&self.path, prev_log_path(&self.path));
        self.open();
    }

    fn append(&mut self, buf: &[u8]) {
        if self.file.is_none() {
            self.open();
        }
        if self.file.is_some() && self.len > 0 && self.len + buf.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut() {
            if f.write_all(buf).is_ok() {
                self.len += buf.len() as u64;
            } else {
                // Reopen on the next write (the file may have been
                // removed or the volume gone away).
                self.file = None;
            }
        }
    }
}

impl RotatingFileWriter {
    pub fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RotState {
                path,
                max_bytes,
                file: None,
                len: 0,
            })),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.lock().path.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RotState> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The fmt layer hands over one whole formatted event per
        // write, so an event never straddles a rotation.
        self.lock().append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(f) = self.lock().file.as_mut() {
            let _ = f.flush();
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingFileWriter {
    type Writer = RotatingFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The app log's last `max` lines, oldest first: `app.log.prev`
/// followed by `app.log`. Empty when neither exists. The diagnostics
/// bundle carries these verbatim (JSON Lines) as `logs/app.log`.
pub fn app_log_lines(data_dir: &Path, max: usize) -> Vec<String> {
    let path = app_log_path(data_dir);
    let mut lines: Vec<String> = Vec::new();
    for p in [prev_log_path(&path), path] {
        if let Ok(bytes) = std::fs::read(&p) {
            lines.extend(String::from_utf8_lossy(&bytes).lines().map(str::to_string));
        }
    }
    if lines.len() > max {
        lines.drain(..lines.len() - max);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-logging-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `init` can be called any number of times; only the first
    /// installs, and every call reports the same outcome.
    #[test]
    fn init_is_idempotent() {
        let dir = temp_dir("init");
        let first = init(&dir);
        let second = init(&dir);
        let third = init_with_layers(&dir, Vec::new());
        assert_eq!(first, second);
        assert_eq!(second, third);
        // Nothing else in this test binary installs a global
        // subscriber, so the pipeline is it, writing to the first
        // call's data dir.
        assert!(first);
        tracing::warn!(probe = "init-idempotent", "global pipeline probe");
        let lines = app_log_lines(&dir, 1000);
        assert!(
            lines.iter().any(|l| l.contains("init-idempotent")),
            "{lines:?}"
        );
        // `dir` stays: the global pipeline keeps logging into it for
        // the rest of this test binary.
    }

    /// Events reach app.log as parseable JSON Lines with level,
    /// message and target.
    #[test]
    fn file_layer_writes_json_lines() {
        let dir = temp_dir("jsonl");
        let writer = RotatingFileWriter::new(app_log_path(&dir), APP_LOG_MAX_BYTES);
        tracing::subscriber::with_default(file_subscriber(writer), || {
            tracing::info!(answer = 42, "hello pipeline");
            tracing::warn!("second line");
        });
        let lines = app_log_lines(&dir, 100);
        assert_eq!(lines.len(), 2, "{lines:?}");
        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["level"], "INFO");
        assert_eq!(first["fields"]["message"], "hello pipeline");
        assert_eq!(first["fields"]["answer"], 42);
        assert!(first["target"].as_str().unwrap().contains("logging"));
        let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(second["level"], "WARN");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Past the size bound the file rotates to `.prev`; the live file
    /// stays within the bound and nothing is lost across one rotation.
    #[test]
    fn rotation_moves_full_file_to_prev() {
        let dir = temp_dir("rotate");
        let path = app_log_path(&dir);
        let mut w = RotatingFileWriter::new(path.clone(), 100);
        for i in 0..5 {
            w.write_all(format!("line-{i:02}-{}\n", "x".repeat(20)).as_bytes())
                .unwrap();
        }
        let prev = prev_log_path(&path);
        assert!(prev.exists(), "rotated file missing");
        assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        assert!(std::fs::metadata(&prev).unwrap().len() <= 100);
        let lines = app_log_lines(&dir, 100);
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(lines[0].starts_with("line-00"));
        assert!(lines[4].starts_with("line-04"));
        // The cap keeps the newest lines.
        let tail = app_log_lines(&dir, 2);
        assert_eq!(tail.len(), 2);
        assert!(tail[1].starts_with("line-04"));
        // A further rotation replaces the old .prev.
        for i in 5..10 {
            w.write_all(format!("line-{i:02}-{}\n", "x".repeat(20)).as_bytes())
                .unwrap();
        }
        let lines = app_log_lines(&dir, 100);
        assert!(!lines.iter().any(|l| l.starts_with("line-00")), "{lines:?}");
        assert!(lines.last().unwrap().starts_with("line-09"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// No log files: no lines (the bundle then omits the scope).
    #[test]
    fn app_log_lines_empty_without_files() {
        let dir = temp_dir("empty");
        assert!(app_log_lines(&dir, 10).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
