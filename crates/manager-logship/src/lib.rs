//! manager-logship — the LLMdil log-streaming shipper
//! (log-streaming design, D9).
//!
//! Tails the app's own log files (MVP: wizard + optimize, §8),
//! redacts every line in-process before anything is serialized to
//! the wire (§5), batches records (§6.4), and delivers them to the
//! single configured destination — the self-hosted receiver (D1) —
//! spooling to disk while the destination is unreachable.
//!
//! The shipper is owned by `manager-app`, started after config load
//! when `log_streaming.enabled` is set, and stopped on shutdown.
//! It is failure-isolated (§6.6): it cannot block, fail, or take
//! down the app.

pub mod record;
pub mod redact;
pub mod secrets;
pub mod shipper;
pub mod spool;
pub mod tail;

pub use record::{classify_level, ShipLevel, WireRecord};
pub use redact::Redactor;
pub use shipper::{spawn, ShipperConfig, ShipperHandle, ShipperStatus};
pub use spool::DEFAULT_SPOOL_CAP_BYTES;

/// The deployment's machine identity when none is configured: the
/// OS hostname (`COMPUTERNAME` on Windows, `HOSTNAME` elsewhere),
/// falling back to `"unknown"`.
pub fn default_machine_name() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let v = s.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "unknown".to_string()
}
