//! The shipped record (design §6.3) and the level heuristic.
//!
//! One JSON object per record; batches are JSON arrays of these,
//! gzip-compressed. `level` uses the same heuristic classification
//! `get_logs` applies (manager-supervisor's `LogLevelFilter`
//! tokens), computed once here so every consumer agrees.

use serde::{Deserialize, Serialize};

/// Wire-format version stamped on every record (`v` field).
pub const WIRE_VERSION: u32 = 1;

/// Severity of a shipped record. Rank ordering matches
/// `manager_config::LogStreamingLevel` (Debug < Info < Warn <
/// Error); errors ship at every floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShipLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl ShipLevel {
    pub fn rank(self) -> u8 {
        match self {
            Self::Debug => 0,
            Self::Info => 1,
            Self::Warn => 2,
            Self::Error => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Classify a raw log line with the `get_logs` heuristic tokens
/// (manager-supervisor `LogLevelFilter::matches_line`): error when
/// the line carries an error/fail/panic/fatal token, warn on
/// "warn", debug on "debug"/"trace", info on "info" — and info as
/// the default for unmarked lines, matching how the Logs view
/// presents them.
pub fn classify_level(line: &str) -> ShipLevel {
    let lc = line.to_lowercase();
    if ["error", "fail", "panic", "fatal"].iter().any(|t| lc.contains(t)) {
        return ShipLevel::Error;
    }
    if lc.contains("warn") {
        return ShipLevel::Warn;
    }
    if lc.contains("debug") || lc.contains("trace") {
        return ShipLevel::Debug;
    }
    ShipLevel::Info
}

/// One shipped log record (design §6.3 wire format).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRecord {
    pub v: u32,
    pub machine: String,
    pub app_version: String,
    /// Per-machine monotonic sequence, persisted across restarts.
    /// Gaps tell a reader records were dropped or the machine was
    /// down; the receiver dedupes on `(machine, seq)`.
    pub seq: u64,
    /// Wall-clock timestamp (local, RFC 3339) for humans and
    /// cross-machine correlation.
    pub ts_wall: String,
    /// Milliseconds on the shipper's monotonic clock (since shipper
    /// start) — ordering and stall detection immune to clock steps.
    pub ts_mono_ms: u64,
    pub source: String,
    pub stream: String,
    pub level: ShipLevel,
    /// The line, redacted (design §5). Redaction happens before a
    /// record is ever constructed.
    pub line: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_get_logs_tokens() {
        assert_eq!(classify_level("ERROR: bind failed"), ShipLevel::Error);
        assert_eq!(classify_level("panic in thread"), ShipLevel::Error);
        assert_eq!(classify_level("warning: slow disk"), ShipLevel::Warn);
        assert_eq!(classify_level("debug: cache miss"), ShipLevel::Debug);
        assert_eq!(classify_level("trace: enter fn"), ShipLevel::Debug);
        assert_eq!(classify_level("info: ready"), ShipLevel::Info);
        assert_eq!(classify_level("=== step start ==="), ShipLevel::Info);
    }

    #[test]
    fn record_serializes_in_wire_shape() {
        let r = WireRecord {
            v: WIRE_VERSION,
            machine: "prometheus".into(),
            app_version: "0.1.50".into(),
            seq: 7,
            ts_wall: "2026-10-19T20:15:35.926-04:00".into(),
            ts_mono_ms: 1234,
            source: "wizard".into(),
            stream: "stdout".into(),
            level: ShipLevel::Info,
            line: "hello".into(),
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["v"], 1);
        assert_eq!(j["level"], "info");
        assert_eq!(j["seq"], 7);
    }
}
