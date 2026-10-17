//! NVML-backed telemetry source (primary).
//!
//! Production wiring goes through `nvml-wrapper`. NVML is loaded **at
//! runtime**, not link time, because the app must start and run its
//! nvidia-smi fallback on machines where NVML is absent or broken
//! (Pascal-era quirks on the P100s are the documented case).
//!
//! ## Runtime-load path (Windows)
//!
//! ```text
//! NvmlSource::try_init()
//!   └─ load_nvml_library()
//!        └─ libloading::Library::new("nvml.dll")          // <-- FFI SEAM
//!             └─ nvml-wrapper init from the loaded library
//!                (nvml-wrapper expects a statically linked libnvml;
//!                 production code wraps its init behind the dynamically
//!                 loaded handle, or shells the equivalent device queries)
//! ```
//!
//! Until that wiring lands, `try_init` records the miss and every poll
//! returns [`TelemetryError::NvmlUnavailable`], which is exactly the
//! signal the [`crate::store::FallbackSource`] uses to switch to
//! `nvidia-smi`. The trait boundary is the seam: no caller outside this
//! module touches NVML directly, so swapping the stub for the real loader
//! changes no other code.

use async_trait::async_trait;
#[cfg(test)]
use chrono::Utc;
use std::sync::Mutex;

use crate::{GpuSample, ProcessSample, TelemetryError, TelemetrySource, TelemetrySourceKind};

#[derive(Debug)]
enum NvmlState {
    /// `try_init` has not run yet.
    Uninit,
    /// Init was attempted and failed; the reason is cached so we do not
    /// retry a doomed load on every poll tick.
    Failed(String),
    /// Library loaded and `nvml-wrapper` initialized.
    Ready,
}

/// NVML-backed source (primary).
pub struct NvmlSource {
    state: Mutex<NvmlState>,
}

impl NvmlSource {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(NvmlState::Uninit),
        }
    }

    /// Attempt the runtime NVML load. Idempotent: a previous failure is
    /// cached and returned without retrying.
    pub fn try_init(&self) -> Result<(), TelemetryError> {
        // A poisoned mutex means a previous holder panicked; the state enum
        // is still readable, so recover rather than panicking the poll loop.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            NvmlState::Ready => Ok(()),
            NvmlState::Failed(reason) => Err(TelemetryError::NvmlUnavailable(reason.clone())),
            NvmlState::Uninit => match Self::load_nvml_library() {
                Ok(()) => {
                    *state = NvmlState::Ready;
                    Ok(())
                }
                Err(e @ TelemetryError::NvmlUnavailable(_)) => {
                    let reason = e.to_string();
                    *state = NvmlState::Failed(reason.clone());
                    Err(TelemetryError::NvmlUnavailable(reason))
                }
                Err(e) => Err(e),
            },
        }
    }

    /// Reports whether a previous `try_init` succeeded.
    pub fn is_ready(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(|e| e.into_inner()),
            NvmlState::Ready
        )
    }

    /// The FFI seam. Production implementation on Windows:
    /// `libloading::Library::new("nvml.dll")`, then initialize
    /// `nvml-wrapper` against the loaded handle and stash device handles.
    fn load_nvml_library() -> Result<(), TelemetryError> {
        // --- FFI SEAM: replace this stub with the real dynamic load. ---
        #[cfg(windows)]
        {
            Err(TelemetryError::NvmlUnavailable(
                "nvml.dll runtime load not yet wired; integrate nvml-wrapper here".to_string(),
            ))
        }
        #[cfg(not(windows))]
        {
            Err(TelemetryError::NvmlUnavailable(
                "NVML is only available on Windows builds of this app".to_string(),
            ))
        }
    }

    fn require_ready(&self) -> Result<(), TelemetryError> {
        self.try_init()
    }
}

impl Default for NvmlSource {
    fn default() -> Self {
        Self::new()
    }
}

// All state is behind `std::sync::Mutex`, so the source is Send + Sync
// automatically; no manual impls needed (and none wanted — a future
// non-thread-safe NVML handle field must fail to compile loudly here).

#[async_trait]
impl TelemetrySource for NvmlSource {
    fn kind(&self) -> TelemetrySourceKind {
        TelemetrySourceKind::Nvml
    }

    async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
        self.require_ready()?;
        // Production: iterate nvml-wrapper device handles, build GpuSamples
        // stamped with Utc::now(). Per-GPU handle failure must degrade to
        // per-GPU staleness, not fail the whole poll.
        Ok(Vec::new())
    }

    async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
        self.require_ready()?;
        // Production: nvml-wrapper per-process accounting stats.
        Ok(Vec::new())
    }
}

/// Convenience constructor for tests that need a timestamped sample.
#[cfg(test)]
pub(crate) fn test_sample(index: u32) -> GpuSample {
    let mut s = GpuSample::fresh(
        index,
        "test-gpu",
        10.0,
        1024,
        16384,
        50.0,
        60.0,
        TelemetrySourceKind::Nvml,
    );
    s.ts = Utc::now();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_init_reports_unavailable_without_nvml() {
        let src = NvmlSource::new();
        let err = src.try_init().expect_err("no NVML on this machine");
        assert!(matches!(err, TelemetryError::NvmlUnavailable(_)));
        assert!(!src.is_ready());
    }

    #[test]
    fn init_failure_is_cached() {
        let src = NvmlSource::new();
        let first = src.try_init().expect_err("expected failure").to_string();
        let second = src
            .try_init()
            .expect_err("expected cached failure")
            .to_string();
        assert_eq!(
            first, second,
            "failure reason must be cached, not recomputed"
        );
    }

    #[tokio::test]
    async fn sample_without_nvml_errors() {
        let src = NvmlSource::new();
        let err = src.sample().await.expect_err("expected NvmlUnavailable");
        assert!(matches!(err, TelemetryError::NvmlUnavailable(_)));
        let err = src.processes().await.expect_err("expected NvmlUnavailable");
        assert!(matches!(err, TelemetryError::NvmlUnavailable(_)));
    }

    #[test]
    fn kind_is_nvml() {
        assert_eq!(NvmlSource::new().kind(), TelemetrySourceKind::Nvml);
    }
}
