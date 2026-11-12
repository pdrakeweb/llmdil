//! In-process record feed (Phase 2 sources S5 and S1's process
//! mode): the app hands the shipper records directly instead of
//! writing them to a file just to tail it.
//!
//! The feed is a bounded broadcast channel. Sending never blocks
//! and never fails the sender: with no shipper subscribed a send is
//! a no-op, and a shipper that falls more than [`RecordTap::CAPACITY`]
//! records behind loses the oldest — it learns the exact count on
//! its next receive and ships that count as one warning record, so
//! the loss is reported rather than silent.

use tokio::sync::broadcast;

use crate::record::ShipLevel;

/// One record offered to the shipper. The shipper redacts the line,
/// applies the level floor, sequences it and stamps the wire fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapRecord {
    pub source: String,
    pub stream: String,
    /// The producer's own level when it knows one; `None` lets the
    /// shipper classify the line.
    pub level: Option<ShipLevel>,
    pub line: String,
}

/// The sending side of the feed. Cheap to clone; every clone feeds
/// the same shipper.
#[derive(Debug, Clone)]
pub struct RecordTap {
    tx: broadcast::Sender<TapRecord>,
}

impl RecordTap {
    /// Records buffered for a subscribed shipper before the oldest
    /// are dropped (and counted).
    pub const CAPACITY: usize = 1024;

    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(Self::CAPACITY);
        Self { tx }
    }

    /// Whether a shipper is subscribed. Producers check this before
    /// building a record so a deployment that never enabled
    /// streaming pays nothing.
    pub fn is_active(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Offer one record. Never blocks; a no-op with no subscriber.
    pub fn send(&self, record: TapRecord) {
        let _ = self.tx.send(record);
    }

    /// The receiving side: the shipper's subscription (and tests').
    pub fn subscribe(&self) -> broadcast::Receiver<TapRecord> {
        self.tx.subscribe()
    }
}

impl Default for RecordTap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: usize) -> TapRecord {
        TapRecord {
            source: "app".into(),
            stream: "stdout".into(),
            level: None,
            line: format!("event {n}"),
        }
    }

    #[test]
    fn send_without_subscriber_is_a_noop() {
        let tap = RecordTap::new();
        assert!(!tap.is_active());
        tap.send(rec(0));
        // A later subscriber sees nothing sent before it subscribed.
        let mut rx = tap.subscribe();
        assert!(tap.is_active());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn overflow_never_blocks_and_reports_the_dropped_count() {
        let tap = RecordTap::new();
        let mut rx = tap.subscribe();
        let extra = 37;
        for n in 0..RecordTap::CAPACITY + extra {
            tap.send(rec(n));
        }
        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(n)) => assert_eq!(n as usize, extra),
            other => panic!("expected Lagged, got {other:?}"),
        }
        // The newest CAPACITY records are still there, in order.
        assert_eq!(rx.try_recv().unwrap(), rec(extra));
    }
}
