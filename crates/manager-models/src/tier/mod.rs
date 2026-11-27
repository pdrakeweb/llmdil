//! Tiered model storage: the crash-safe verified copy, the state file,
//! volume queries, and the promotion/demotion policy
//! (docs/tiered-model-storage.md). The resolver and the configuration
//! live in manager-config (`storage`); orchestration — the reconcile
//! loop, operations, events — lives in manager-app (`model_storage`).

mod copy;
mod policy;
mod state;
mod volume;

pub use copy::{copy_verified, sweep_partials, CopyControl, CopyOptions, CopyOutcome, CopyPhase};
pub use policy::{plan, FileFacts, MoveKind, PinReason, PlanInputs, PlannedMove, SkippedMove};
pub use state::{
    file_mtime_ms, ArchiveRecord, MoveOutcome, MoveRecord, StorageState, UsageRecord,
    MOVE_HISTORY_CAP, STATE_FILE, STATE_VERSION,
};
pub use volume::{free_bytes, same_volume, total_bytes};

/// Bytes in one GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// Head-room a copy needs on its destination beyond the file itself.
pub const COPY_HEADROOM_BYTES: u64 = 2 * GIB;

/// Errors from a tier copy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TierError {
    #[error("I/O error: {0}")]
    Io(String),
    #[error("the destination volume is full")]
    DiskFull,
    #[error("not enough free space on the destination: {needed} bytes needed, {free} free")]
    NoSpace { needed: u64, free: u64 },
    #[error("cancelled")]
    Cancelled,
    /// The source file's own bytes do not match the expected hash: the
    /// file being moved is bad (not a copy error).
    #[error("the source file does not match its SHA-256 pin (expected {expected}, got {actual})")]
    SourceMismatch { expected: String, actual: String },
    /// The copy on the destination does not match: a write error.
    #[error("the copy does not match the source (expected {expected}, got {actual})")]
    DestMismatch { expected: String, actual: String },
}

impl TierError {
    pub(crate) fn from_io(e: std::io::Error) -> Self {
        if is_disk_full(&e) {
            TierError::DiskFull
        } else {
            TierError::Io(e.to_string())
        }
    }
}

/// Whether an I/O error means "volume full" (Windows ERROR_DISK_FULL /
/// ERROR_HANDLE_DISK_FULL, POSIX ENOSPC).
pub(crate) fn is_disk_full(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::StorageFull {
        return true;
    }
    match e.raw_os_error() {
        #[cfg(windows)]
        Some(112) | Some(39) => true,
        #[cfg(not(windows))]
        Some(28) => true,
        _ => false,
    }
}
