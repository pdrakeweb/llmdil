//! Test helpers shared by the unit tests of several `lib.rs` op modules.

use crate::*;

pub(crate) fn test_cfg() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.data_dir = PathBuf::from("/data");
    cfg
}

/// A fresh scratch directory name: pid + a process-wide counter +
/// the clock. The clock alone collided between tests started in
/// parallel, letting two tests share (and one delete) a data dir.
pub(crate) fn unique_test_dir(prefix: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{n}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// An AppState over a temp dir for the advanced-flags op
/// tests (tracker #60): config file and data dir in scratch
/// space, so saves and the activation journal land there.
pub(crate) fn advanced_test_state(cfg: AppConfig) -> (Arc<AppState>, PathBuf) {
    let dir = unique_test_dir("llm-mgr-advanced-flags");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = cfg;
    cfg.data_dir = dir.clone();
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    // Pin an empty telemetry store. Left unset, the first
    // `state.telemetry()` call starts the real host poller and
    // an nvidia-smi inventory query, and whichever finishes
    // first decides what the op sees: on a GPU host the GPU
    // count/names arrived mid-test only under load, flipping
    // split checks and measurement fingerprints. A test that
    // needs GPUs writes a fixture inventory into this store.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    (state, dir)
}

pub(crate) fn test_sink() -> Arc<dyn EventSink> {
    Arc::new(NoopSink)
}
