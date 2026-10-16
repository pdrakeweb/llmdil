//! Snapshot store, poll loop, and source fallback.
//!
//! [`TelemetryStore`] is the single-writer store fed by the poll loop.
//! [`Poller`] runs the loop at a [`PollCadence`]. [`FallbackSource`] tries
//! the primary source first and falls back to the secondary on error,
//! recording which source produced the last successful poll.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    GpuInfo, GpuSample, ProcessSample, TelemetryError, TelemetrySource, TelemetrySourceKind,
    STALE_AFTER_SECS,
};

/// How many samples per GPU the history ring keeps. At the default 1 s
/// cadence this is two minutes of sparkline data (spec asks for 60 s;
/// the extra headroom covers the 5 s cadence without resizing).
pub const HISTORY_CAP: usize = 120;

/// One resettable peak mark: the extreme value plus when it was recorded
/// (spec §5: the UI shows peak marks with timestamps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeakMark<T> {
    pub value: T,
    pub at: DateTime<Utc>,
}

/// Resettable peak marks per GPU (spec §5 derived state).
///
/// Each mark is `None` until the first sample raises it, so a missing mark
/// is never confused with a zero reading.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeakMarks {
    /// Max GPU utilization %.
    pub utilization_pct: Option<PeakMark<f32>>,
    /// Max VRAM utilization % (used / total of the same sample).
    pub vram_util_pct: Option<PeakMark<f32>>,
    /// Max VRAM used, MiB.
    pub vram_used_mib: Option<PeakMark<u64>>,
    /// Max temperature, C.
    pub temp_c: Option<PeakMark<f32>>,
    /// Max board power, W.
    pub power_w: Option<PeakMark<f32>>,
}

/// Raise one peak mark when `value` is a new maximum (or the first value).
fn raise_mark<T: PartialOrd>(slot: &mut Option<PeakMark<T>>, value: T, at: DateTime<Utc>) {
    let higher = match slot {
        Some(mark) => value > mark.value,
        None => true,
    };
    if higher {
        *slot = Some(PeakMark { value, at });
    }
}

/// One failed poll, as recorded by the poll loop: the error message plus
/// when it happened. Kept until the next successful poll clears it, so
/// readers can distinguish "the telemetry source is failing" from "this
/// machine has no GPUs" — two states an empty snapshot alone conflates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollError {
    pub message: String,
    pub at: DateTime<Utc>,
}

struct StoredSample {
    sample: GpuSample,
    /// Monotonic instant of the last successful poll for this GPU.
    last_ok: Instant,
}

/// In-memory snapshot store: single writer (the poll loop), many readers.
///
/// - `update` records a fresh poll; reported GPUs leave the STALE state.
/// - `snapshot` returns last-known values with `stale: true` applied to any
///   GPU older than [`STALE_AFTER_SECS`]. Values are never synthesized.
/// - Per-GPU history rings back the sparkline views; peak marks are
///   resettable via [`TelemetryStore::reset_peaks`].
pub struct TelemetryStore {
    latest: HashMap<u32, StoredSample>,
    history: HashMap<u32, VecDeque<GpuSample>>,
    peaks: HashMap<u32, PeakMarks>,
    inventory: HashMap<u32, GpuInfo>,
    last_processes: Vec<ProcessSample>,
    /// Most recent poll failure not yet superseded by a success.
    last_error: Option<PollError>,
    /// Wall-clock time of the most recent successful poll.
    last_ok_at: Option<DateTime<Utc>>,
}

impl TelemetryStore {
    pub fn new() -> Self {
        Self {
            latest: HashMap::new(),
            history: HashMap::new(),
            peaks: HashMap::new(),
            inventory: HashMap::new(),
            last_processes: Vec::new(),
            last_error: None,
            last_ok_at: None,
        }
    }

    /// Record a fresh poll; clears STALE for reported GPUs, extends history
    /// rings, and raises peak marks.
    ///
    /// A poll that reached `update` succeeded, so it also stamps the
    /// last-successful-poll time and clears any recorded poll error.
    pub fn update(&mut self, samples: Vec<GpuSample>) {
        self.last_ok_at = Some(Utc::now());
        self.last_error = None;
        let now = Instant::now();
        self.update_at(samples, now);
    }

    /// Record a failed poll (called by the poll loop, the store's only
    /// writer). The error stays visible via [`Self::last_poll_error`]
    /// until the next successful poll clears it.
    pub fn record_poll_error(&mut self, message: String) {
        self.last_error = Some(PollError {
            message,
            at: Utc::now(),
        });
    }

    /// The most recent poll failure, if polls are currently failing
    /// (a later success clears it). `None` before the first poll and
    /// whenever the last poll succeeded.
    pub fn last_poll_error(&self) -> Option<&PollError> {
        self.last_error.as_ref()
    }

    /// Wall-clock time of the most recent successful poll, if any.
    pub fn last_successful_poll_at(&self) -> Option<DateTime<Utc>> {
        self.last_ok_at
    }

    fn update_at(&mut self, samples: Vec<GpuSample>, now: Instant) {
        // A GPU absent from this poll is NOT pruned. It stays in the
        // snapshot, flagged down with its last-known readings: a GPU
        // that falls off the bus ("GPU is lost") must read as down in
        // the UI, not silently vanish (operator requirement,
        // 2026-10-16). Its peak marks and history stay with it. The
        // first poll that reports it again restores it to live.
        let present: std::collections::HashSet<u32> = samples.iter().map(|s| s.index).collect();
        for (idx, stored) in self.latest.iter_mut() {
            if !present.contains(idx) {
                stored.sample.down = true;
            }
        }
        for mut sample in samples {
            sample.stale = false;
            let idx = sample.index;

            let ts = sample.ts;
            let vram_util_pct = if sample.vram_total_mib > 0 {
                sample.vram_used_mib as f32 / sample.vram_total_mib as f32 * 100.0
            } else {
                0.0
            };
            let peak = self.peaks.entry(idx).or_default();
            raise_mark(&mut peak.utilization_pct, sample.utilization_pct, ts);
            raise_mark(&mut peak.vram_util_pct, vram_util_pct, ts);
            raise_mark(&mut peak.vram_used_mib, sample.vram_used_mib, ts);
            raise_mark(&mut peak.temp_c, sample.temp_c, ts);
            // Power is optional: cards that report N/A simply never
            // raise a power peak.
            if let Some(power_w) = sample.power_w {
                raise_mark(&mut peak.power_w, power_w, ts);
            }

            let ring = self.history.entry(idx).or_default();
            ring.push_back(sample.clone());
            while ring.len() > HISTORY_CAP {
                ring.pop_front();
            }

            self.latest.insert(
                idx,
                StoredSample {
                    sample,
                    last_ok: now,
                },
            );
        }
    }

    /// Record the latest per-process attribution (drives the observed
    /// tensor-split derivation upstream).
    pub fn update_processes(&mut self, processes: Vec<ProcessSample>) {
        self.last_processes = processes;
    }

    /// Static inventory (name, UUID, PCI, driver, VRAM per GPU).
    pub fn set_inventory(&mut self, infos: Vec<GpuInfo>) {
        self.inventory = infos.into_iter().map(|i| (i.index, i)).collect();
    }

    pub fn inventory(&self) -> Vec<GpuInfo> {
        let mut infos: Vec<GpuInfo> = self.inventory.values().cloned().collect();
        infos.sort_by_key(|i| i.index);
        infos
    }

    /// Current snapshot; GPUs older than [`STALE_AFTER_SECS`] are returned
    /// with `stale: true` and last-known values. Sorted by GPU index.
    pub fn snapshot(&self) -> Vec<GpuSample> {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> Vec<GpuSample> {
        let mut out: Vec<GpuSample> = self
            .latest
            .values()
            .map(|stored| {
                let mut s = stored.sample.clone();
                s.stale =
                    now.duration_since(stored.last_ok) > Duration::from_secs(STALE_AFTER_SECS);
                s
            })
            .collect();
        out.sort_by_key(|s| s.index);
        out
    }

    /// History ring for one GPU (oldest first), for sparklines.
    pub fn history(&self, index: u32) -> Vec<GpuSample> {
        self.history
            .get(&index)
            .map(|ring| ring.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Peak marks for one GPU, if any samples have been recorded.
    pub fn peaks(&self, index: u32) -> Option<PeakMarks> {
        self.peaks.get(&index).cloned()
    }

    /// Reset all peak marks (spec: resettable).
    pub fn reset_peaks(&mut self) {
        self.peaks.clear();
    }

    pub fn last_processes(&self) -> &[ProcessSample] {
        &self.last_processes
    }

    /// Number of GPUs currently tracked.
    pub fn gpu_count(&self) -> usize {
        self.latest.len()
    }
}

impl Default for TelemetryStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Poll cadence + poller
// ---------------------------------------------------------------------------

/// Poll cadence (spec §5: user-selectable 1 s / 5 s / off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollCadence {
    Every1s,
    Every5s,
    Off,
}

impl PollCadence {
    pub fn interval(self) -> Option<Duration> {
        match self {
            PollCadence::Every1s => Some(Duration::from_secs(1)),
            PollCadence::Every5s => Some(Duration::from_secs(5)),
            PollCadence::Off => None,
        }
    }
}

/// Runs the poll loop: one poller, the store is the single writer.
/// On poll failure the tick is missed for sampling purposes — the
/// store's staleness rule (no successful poll within
/// [`STALE_AFTER_SECS`]) handles labeling — and the error is recorded in
/// the store via [`TelemetryStore::record_poll_error`] so the UI can
/// tell a failing source apart from a GPU-less machine.
pub struct Poller<S> {
    source: S,
    store: Arc<Mutex<TelemetryStore>>,
    cadence: PollCadence,
}

impl<S: TelemetrySource> Poller<S> {
    /// Create a poller and the shared store it writes to. The caller owns
    /// the `Arc` clone for readers (UI snapshot path).
    pub fn new(source: S, cadence: PollCadence) -> (Self, Arc<Mutex<TelemetryStore>>) {
        let store = Arc::new(Mutex::new(TelemetryStore::new()));
        (
            Self {
                source,
                store: Arc::clone(&store),
                cadence,
            },
            store,
        )
    }

    /// Run a single poll cycle now. Returns the number of GPUs reported
    /// (0 on poll failure — the tick is missed, staleness handles the
    /// rest, and the error is recorded in the store). Uses the combined
    /// poll so sources like nvidia-smi pay one acquisition per tick
    /// instead of two.
    pub async fn poll_once(&self) -> usize {
        let (samples, processes) = match self.source.sample_with_processes().await {
            Ok(out) => out,
            Err(e) => {
                if let Ok(mut store) = self.store.lock() {
                    store.record_poll_error(e.to_string());
                }
                return 0;
            }
        };
        let n = samples.len();
        if let Ok(mut store) = self.store.lock() {
            store.update(samples);
            store.update_processes(processes);
        }
        n
    }

    /// Run the loop until `shutdown` is notified. With cadence Off this
    /// performs one initial poll and then waits.
    pub async fn run(self, shutdown: tokio::sync::watch::Receiver<bool>) {
        // One immediate poll so the UI is not empty for a full interval.
        self.poll_once().await;

        let Some(interval) = self.cadence.interval() else {
            wait_for_shutdown(shutdown).await;
            return;
        };

        let mut ticker = tokio::time::interval(interval);
        // `interval` fires immediately on first tick; we already polled once,
        // so skip that first immediate fire.
        ticker.tick().await;
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.poll_once().await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }

    /// Spawn the loop on the Tokio runtime; returns a handle whose `stop`
    /// shuts the loop down.
    pub fn start(self) -> PollHandle
    where
        S: 'static,
    {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let join = tokio::spawn(self.run(rx));
        PollHandle { shutdown: tx, join }
    }
}

/// Wait until the watch channel signals shutdown (or the sender drops).
async fn wait_for_shutdown(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return; // sender dropped: treat as shutdown
        }
    }
}

/// Handle to a running [`Poller`].
pub struct PollHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl PollHandle {
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.join.await;
    }
}

// ---------------------------------------------------------------------------
// Fallback source
// ---------------------------------------------------------------------------

/// Tries `primary` first; on error falls back to `secondary`.
///
/// Records which source produced the last successful poll — that is what
/// the UI shows per GPU ("NVML" / "nvidia-smi fallback"). Recovers back to
/// the primary automatically on the next successful primary poll.
pub struct FallbackSource<P, F> {
    primary: P,
    fallback: F,
    active: Mutex<TelemetrySourceKind>,
}

impl<P: TelemetrySource, F: TelemetrySource> FallbackSource<P, F> {
    pub fn new(primary: P, fallback: F) -> Self {
        let active = primary.kind();
        Self {
            primary,
            fallback,
            active: Mutex::new(active),
        }
    }

    /// Which source produced the last successful poll.
    pub fn active_kind(&self) -> TelemetrySourceKind {
        *self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_active(&self, kind: TelemetrySourceKind) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = kind;
    }
}

#[async_trait]
impl<P: TelemetrySource, F: TelemetrySource> TelemetrySource for FallbackSource<P, F> {
    fn kind(&self) -> TelemetrySourceKind {
        self.active_kind()
    }

    async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
        match self.primary.sample().await {
            Ok(samples) => {
                self.set_active(self.primary.kind());
                Ok(samples)
            }
            Err(_) => {
                let samples = self.fallback.sample().await?;
                self.set_active(self.fallback.kind());
                Ok(samples)
            }
        }
    }

    async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
        match self.primary.processes().await {
            Ok(procs) => {
                self.set_active(self.primary.kind());
                Ok(procs)
            }
            Err(_) => {
                let procs = self.fallback.processes().await?;
                self.set_active(self.fallback.kind());
                Ok(procs)
            }
        }
    }

    async fn sample_with_processes(
        &self,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        // Both result sets come from the same source on a given tick, so
        // the samples and the process attribution can never disagree about
        // which source produced them.
        match self.primary.sample_with_processes().await {
            Ok(out) => {
                self.set_active(self.primary.kind());
                Ok(out)
            }
            Err(_) => {
                let out = self.fallback.sample_with_processes().await?;
                self.set_active(self.fallback.kind());
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvml::test_sample;
    use crate::TelemetrySourceKind;

    struct FakeSource {
        kind: TelemetrySourceKind,
        samples: Vec<GpuSample>,
        fail: bool,
    }

    impl FakeSource {
        fn ok(kind: TelemetrySourceKind, n: u32) -> Self {
            Self {
                kind,
                samples: (0..n).map(test_sample).collect(),
                fail: false,
            }
        }
        fn failing(kind: TelemetrySourceKind) -> Self {
            Self {
                kind,
                samples: Vec::new(),
                fail: true,
            }
        }
    }

    #[async_trait]
    impl TelemetrySource for FakeSource {
        fn kind(&self) -> TelemetrySourceKind {
            self.kind
        }
        async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
            if self.fail {
                Err(TelemetryError::SmiFailed("fake failure".into()))
            } else {
                Ok(self.samples.clone())
            }
        }
        async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
            if self.fail {
                Err(TelemetryError::SmiFailed("fake failure".into()))
            } else {
                Ok(Vec::new())
            }
        }
    }

    fn sample_at(index: u32, secs_ago: u64, temp_c: f32) -> GpuSample {
        let mut s = test_sample(index);
        s.ts = chrono::Utc::now() - chrono::Duration::seconds(secs_ago as i64);
        s.temp_c = temp_c;
        s
    }

    #[test]
    fn staleness_transitions() {
        let mut store = TelemetryStore::new();
        // Anchor all assertions to a single instant captured before the
        // update; last_ok is recorded inside update(), hence >= t0, which
        // makes the boundary arithmetic deterministic.
        let t0 = Instant::now();
        store.update(vec![sample_at(0, 0, 60.0)]);

        // Fresh: not stale.
        let snap = store.snapshot_at(t0 + Duration::from_secs(1));
        assert_eq!(snap.len(), 1);
        assert!(!snap[0].stale);

        // 11 s without a poll: stale, but last-known values preserved.
        let snap = store.snapshot_at(t0 + Duration::from_secs(11));
        assert!(snap[0].stale, "must be stale after STALE_AFTER_SECS");
        assert!(
            (snap[0].temp_c - 60.0).abs() < f32::EPSILON,
            "last-known values kept"
        );

        // Exactly at the boundary: not stale (strictly greater required).
        let snap = store.snapshot_at(t0 + Duration::from_secs(STALE_AFTER_SECS));
        assert!(!snap[0].stale);

        // A fresh poll clears staleness and picks up new values.
        let t1 = Instant::now();
        store.update(vec![sample_at(0, 0, 61.0)]);
        let snap = store.snapshot_at(t1 + Duration::from_secs(1));
        assert!(!snap[0].stale);
        assert!((snap[0].temp_c - 61.0).abs() < f32::EPSILON);
    }

    #[test]
    fn history_ring_is_bounded() {
        let mut store = TelemetryStore::new();
        for i in 0..(HISTORY_CAP + 50) {
            store.update(vec![sample_at(0, 0, i as f32)]);
        }
        let hist = store.history(0);
        assert_eq!(hist.len(), HISTORY_CAP);
        // Oldest entries were evicted: first retained temp is 50.0.
        assert!((hist[0].temp_c - 50.0).abs() < f32::EPSILON);
        assert!((hist[HISTORY_CAP - 1].temp_c - (HISTORY_CAP + 49) as f32).abs() < f32::EPSILON);
    }

    #[test]
    fn history_is_per_gpu() {
        let mut store = TelemetryStore::new();
        store.update(vec![sample_at(0, 0, 60.0), sample_at(1, 0, 50.0)]);
        store.update(vec![sample_at(0, 0, 61.0)]);
        assert_eq!(store.history(0).len(), 2);
        assert_eq!(store.history(1).len(), 1);
        assert!(store.history(7).is_empty());
    }

    #[test]
    fn peaks_track_maxima_and_reset() {
        let mut store = TelemetryStore::new();
        store.update(vec![sample_at(0, 0, 60.0)]);
        let mut s = sample_at(0, 0, 70.0);
        s.power_w = Some(120.0);
        s.vram_used_mib = 8192; // of 16384 total -> 50% VRAM util
        s.utilization_pct = 42.0;
        let peak_ts = s.ts;
        store.update(vec![s]);
        store.update(vec![sample_at(0, 0, 65.0)]);

        let peaks = store.peaks(0).expect("peaks recorded");
        let temp = peaks.temp_c.expect("temp peak");
        assert!((temp.value - 70.0).abs() < f32::EPSILON);
        assert_eq!(temp.at, peak_ts, "peak carries the sample timestamp");
        assert!((peaks.power_w.expect("power peak").value - 120.0).abs() < f32::EPSILON);
        assert_eq!(peaks.vram_used_mib.expect("vram peak").value, 8192);
        let vram_util = peaks.vram_util_pct.expect("vram util peak");
        assert!((vram_util.value - 50.0).abs() < 1e-3);
        assert!((peaks.utilization_pct.expect("util peak").value - 42.0).abs() < f32::EPSILON);

        // A later, lower sample must not move the marks.
        let peaks2 = store.peaks(0).expect("peaks recorded");
        assert_eq!(peaks2.temp_c.unwrap().at, peak_ts);

        store.reset_peaks();
        assert!(store.peaks(0).is_none());
    }

    #[test]
    fn peaks_absent_before_first_sample() {
        let store = TelemetryStore::new();
        assert!(store.peaks(0).is_none());
    }

    #[test]
    fn snapshot_sorted_by_index() {
        let mut store = TelemetryStore::new();
        store.update(vec![
            sample_at(2, 0, 50.0),
            sample_at(0, 0, 60.0),
            sample_at(1, 0, 55.0),
        ]);
        let snap = store.snapshot();
        let idx: Vec<u32> = snap.iter().map(|s| s.index).collect();
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn update_replaces_latest_per_gpu() {
        let mut store = TelemetryStore::new();
        store.update(vec![sample_at(0, 0, 60.0)]);
        store.update(vec![sample_at(0, 0, 75.0)]);
        assert_eq!(store.gpu_count(), 1);
        let snap = store.snapshot();
        assert!((snap[0].temp_c - 75.0).abs() < f32::EPSILON);
    }

    #[test]
    fn update_marks_vanished_gpus_down() {
        let mut store = TelemetryStore::new();
        store.update(vec![sample_at(0, 0, 60.0), sample_at(1, 0, 50.0)]);
        assert_eq!(store.gpu_count(), 2);
        // GPU 1 vanishes from the next poll (fallen off the bus): it
        // stays in the snapshot, flagged down, with last-known values
        // and its peak marks — it must not silently disappear.
        store.update(vec![sample_at(0, 0, 61.0)]);
        assert_eq!(store.gpu_count(), 2);
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[1].index, 1);
        assert!(snap[1].down, "vanished GPU is flagged down");
        assert!(
            (snap[1].temp_c - 50.0).abs() < f32::EPSILON,
            "last-known readings are kept"
        );
        assert!(!snap[0].down);
        assert!(store.peaks(1).is_some(), "peaks stay with the down GPU");
        // The first poll that reports it again restores it to live.
        store.update(vec![sample_at(0, 0, 62.0), sample_at(1, 0, 55.0)]);
        let snap = store.snapshot();
        assert!(!snap[1].down, "a reported GPU is live again");
        assert!((snap[1].temp_c - 55.0).abs() < f32::EPSILON);
    }

    #[test]
    fn inventory_round_trip() {
        let mut store = TelemetryStore::new();
        assert!(store.inventory().is_empty());
        store.set_inventory(vec![
            crate::GpuInfo {
                index: 1,
                name: "b".into(),
                uuid: "u1".into(),
                compute_capability: (6, 0),
                total_vram_mib: 16384,
                pci_bus_id: "00000000:65:00.0".into(),
                driver_version: "581.57".into(),
                source: TelemetrySourceKind::NvidiaSmi,
            },
            crate::GpuInfo {
                index: 0,
                name: "a".into(),
                uuid: "u0".into(),
                compute_capability: (8, 6),
                total_vram_mib: 16384,
                pci_bus_id: "00000000:01:00.0".into(),
                driver_version: "581.57".into(),
                source: TelemetrySourceKind::NvidiaSmi,
            },
        ]);
        let inv = store.inventory();
        assert_eq!(inv.len(), 2);
        assert_eq!(inv[0].index, 0, "inventory sorted by index");
    }

    #[tokio::test]
    async fn fallback_uses_primary_when_healthy() {
        let fb = FallbackSource::new(
            FakeSource::ok(TelemetrySourceKind::Nvml, 2),
            FakeSource::ok(TelemetrySourceKind::NvidiaSmi, 3),
        );
        let samples = fb.sample().await.expect("primary ok");
        assert_eq!(samples.len(), 2);
        assert_eq!(fb.active_kind(), TelemetrySourceKind::Nvml);
        assert_eq!(fb.kind(), TelemetrySourceKind::Nvml);
    }

    #[tokio::test]
    async fn fallback_switches_to_secondary_on_primary_failure() {
        let fb = FallbackSource::new(
            FakeSource::failing(TelemetrySourceKind::Nvml),
            FakeSource::ok(TelemetrySourceKind::NvidiaSmi, 3),
        );
        assert_eq!(
            fb.active_kind(),
            TelemetrySourceKind::Nvml,
            "starts as primary"
        );
        let samples = fb.sample().await.expect("fallback ok");
        assert_eq!(samples.len(), 3);
        assert_eq!(fb.active_kind(), TelemetrySourceKind::NvidiaSmi);
    }

    #[tokio::test]
    async fn fallback_errors_when_both_fail() {
        let fb = FallbackSource::new(
            FakeSource::failing(TelemetrySourceKind::Nvml),
            FakeSource::failing(TelemetrySourceKind::NvidiaSmi),
        );
        fb.sample().await.expect_err("both failing must error");
        // Active kind stays primary: nothing has produced a successful poll.
        assert_eq!(fb.active_kind(), TelemetrySourceKind::Nvml);
    }

    #[tokio::test]
    async fn fallback_recovers_to_primary() {
        let fb = FallbackSource::new(
            FakeSource::failing(TelemetrySourceKind::Nvml),
            FakeSource::ok(TelemetrySourceKind::NvidiaSmi, 1),
        );
        fb.sample().await.expect("fallback ok");
        assert_eq!(fb.active_kind(), TelemetrySourceKind::NvidiaSmi);
        // Swap in a healthy primary by rebuilding (interior state is per-instance).
        let fb2 = FallbackSource::new(
            FakeSource::ok(TelemetrySourceKind::Nvml, 1),
            FakeSource::ok(TelemetrySourceKind::NvidiaSmi, 1),
        );
        fb2.sample().await.expect("primary ok");
        assert_eq!(fb2.active_kind(), TelemetrySourceKind::Nvml);
    }

    #[tokio::test]
    async fn poll_once_updates_store() {
        let (poller, store) = Poller::new(
            FakeSource::ok(TelemetrySourceKind::Nvml, 2),
            PollCadence::Every1s,
        );
        assert_eq!(poller.poll_once().await, 2);
        let snap = store.lock().unwrap().snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|s| !s.stale));
    }

    #[tokio::test]
    async fn poll_once_failure_misses_tick() {
        let (poller, store) = Poller::new(
            FakeSource::failing(TelemetrySourceKind::Nvml),
            PollCadence::Every1s,
        );
        assert_eq!(poller.poll_once().await, 0);
        assert!(store.lock().unwrap().snapshot().is_empty());
    }

    #[tokio::test]
    async fn poll_error_is_recorded_and_cleared_by_success() {
        // A source that fails until flipped healthy.
        struct FlakySource {
            fail: std::sync::atomic::AtomicBool,
        }
        #[async_trait]
        impl TelemetrySource for FlakySource {
            fn kind(&self) -> TelemetrySourceKind {
                TelemetrySourceKind::Nvml
            }
            async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
                if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                    Err(TelemetryError::SmiFailed("flaky failure".into()))
                } else {
                    Ok(vec![test_sample(0)])
                }
            }
            async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
                Ok(Vec::new())
            }
        }

        let (poller, store) = Poller::new(
            FlakySource {
                fail: std::sync::atomic::AtomicBool::new(true),
            },
            PollCadence::Every1s,
        );

        // Failing poll: no samples, but the failure is now visible in the
        // store — distinguishable from a GPU-less machine's empty snapshot.
        assert_eq!(poller.poll_once().await, 0);
        {
            let store = store.lock().unwrap();
            let err = store.last_poll_error().expect("error recorded");
            assert!(err.message.contains("flaky failure"), "{}", err.message);
            assert!(store.last_successful_poll_at().is_none());
        }

        // Next poll succeeds: error cleared, success time stamped.
        poller
            .source
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(poller.poll_once().await, 1);
        let store = store.lock().unwrap();
        assert!(store.last_poll_error().is_none());
        assert!(store.last_successful_poll_at().is_some());
    }

    #[tokio::test]
    async fn poller_run_ticks_and_stops() {
        let (poller, store) = Poller::new(
            FakeSource::ok(TelemetrySourceKind::Nvml, 1),
            PollCadence::Every1s,
        );
        let handle = poller.start();
        // Immediate poll + at least one 1 s tick.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        handle.stop().await;
        let hist_len = store.lock().unwrap().history(0).len();
        assert!(
            hist_len >= 2,
            "expected initial + ticked polls, got {hist_len}"
        );
    }

    #[test]
    fn cadence_intervals() {
        assert_eq!(
            PollCadence::Every1s.interval(),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            PollCadence::Every5s.interval(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(PollCadence::Off.interval(), None);
    }

    #[test]
    fn free_vram_clamps() {
        let mut s = test_sample(0);
        s.vram_used_mib = 2048;
        s.vram_total_mib = 16384;
        assert_eq!(s.free_vram_mib(), 14336);
        s.vram_used_mib = 20000;
        assert_eq!(s.free_vram_mib(), 0, "never negative");
    }
}
