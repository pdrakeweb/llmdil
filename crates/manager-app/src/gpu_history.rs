//! Persistent per-GPU telemetry history (temperature, utilization,
//! throughput, power) behind the GPUs page history charts.
//!
//! The telemetry poll loop keeps only a two-minute in-memory ring
//! (sparkline data); nothing survived an app restart, and nothing
//! recorded throughput at all. This module is the durable tier: a
//! recorder (driven from the desktop shell on the gateway-metrics
//! cadence, see [`record_gpu_history_once`]) samples the telemetry
//! store's latest snapshot every [`SAMPLE_INTERVAL_MS`], attributes
//! per-backend throughput to GPUs, and appends one point per GPU to
//! a JSON store in the data dir (`gpu-history.json`).
//!
//! Retention is two-tier and bounded by time, so the caps below are
//! also hard count caps:
//! - recent tier: full-rate points (one per 15 s) for 48 h —
//!   at most [`RECENT_CAP`] points per GPU;
//! - archive tier: 5-minute bucket averages for 7 days — at most
//!   [`ARCHIVE_CAP`] points per GPU.
//! A point serializes to roughly 60 bytes of compact JSON, so the
//! worst case is ~0.8 MB per GPU (~3.3 MB on a four-GPU machine).
//! Points that age out of the recent tier are folded into archive
//! buckets (averaged), never dropped silently inside the retention
//! window, and archive points older than 7 days are pruned.
//!
//! Throughput attribution: the per-backend token rate is the gateway
//! stats store's latest interval rate (the same figure the Dashboard
//! shows as a backend's throughput). Only backends with a live
//! supervised process contribute — a stopped backend's gateway
//! counters freeze, and a frozen counter is not a measurement. A
//! single-GPU placement attributes the full rate to that GPU; a
//! split placement pro-rates by the tensor-split shares (normalized
//! over the placed GPUs). CPU-only and undetermined placements
//! contribute nowhere. A GPU no running backend contributes to gets
//! NO throughput value for that tick (absent, never zero): unknown
//! is not zero (design principle 12).
//!
//! Power comes from the telemetry sample itself
//! (`GpuSample::power_w`): recorded when the card reports it, absent
//! when the driver answers [N/A] — again absent, never zero.
//!
//! Discipline mirrors [`crate::activation_history`]: a missing or
//! corrupt file loads as an empty history, saves are tmp-file +
//! rename, and recording is best-effort — a history failure must
//! never fail or stall telemetry. Points are recorded only from
//! fresh samples: a GPU absent from the latest poll (flagged down)
//! or carrying a sample older than one interval (the source is
//! failing and the snapshot is last-known) contributes no point, so
//! an outage reads as a gap in the charts, not a flat line of
//! repeated last-known values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use manager_telemetry::GpuSample;

use crate::{ApiError, ApiResult, AppState, EventSink};

/// Recorder cadence: one point per GPU per 15 s, aligned with the
/// gateway metrics scrape (the throughput series' interval rate is
/// per-scrape, so a faster history cadence would only repeat it).
pub const SAMPLE_INTERVAL_MS: i64 = 15_000;

/// How long full-rate points are kept: 48 h.
pub const RECENT_RETENTION_MS: i64 = 48 * 3_600_000;

/// Hard cap on recent points per GPU (48 h at 15 s, plus slack for
/// a recorder that ticks slightly fast).
pub const RECENT_CAP: usize = 11_600;

/// Archive bucket width: 5 minutes.
pub const ARCHIVE_BUCKET_MS: i64 = 300_000;

/// How long archive points are kept: 7 days.
pub const ARCHIVE_RETENTION_MS: i64 = 7 * 24 * 3_600_000;

/// Hard cap on archive points per GPU (7 days at 5 min, plus slack).
pub const ARCHIVE_CAP: usize = 2_100;

/// Most points one query response carries per GPU; wider windows
/// are decimated by consecutive-group averaging to fit.
pub const MAX_QUERY_POINTS: usize = 600;

/// Persistence is throttled: the file is rewritten at most once a
/// minute, so a crash loses at most a minute of history (the
/// app-off period is a chart gap either way).
pub const SAVE_INTERVAL_MS: i64 = 60_000;

/// One recorded sample for one GPU. `throughput_toks` and `power_w`
/// are `None` when the value was not measured at that tick —
/// serialized as null, rendered as a gap, never as zero.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HistoryPoint {
    pub ts_ms: i64,
    pub temp_c: f32,
    pub util_pct: f32,
    pub throughput_toks: Option<f32>,
    pub power_w: Option<f32>,
}

/// Per-GPU series: the two retention tiers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuSeries {
    /// Last-seen GPU name (the series is keyed by GPU index, the
    /// app's telemetry identity; the name rides along for labels).
    #[serde(default)]
    pub name: String,
    /// Full-rate points, oldest first.
    #[serde(default)]
    pub recent: Vec<HistoryPoint>,
    /// 5-minute averaged points, oldest first.
    #[serde(default)]
    pub archive: Vec<HistoryPoint>,
}

/// The persisted history file: series by GPU index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuHistoryStore {
    #[serde(default)]
    pub gpus: HashMap<u32, GpuSeries>,
}

/// Where a backend's throughput lands.
#[derive(Debug, Clone, PartialEq)]
pub enum GpuPlacement {
    /// The whole rate belongs to this GPU.
    Single(u32),
    /// (GPU index, share) pairs; shares sum to 1.
    Split(Vec<(u32, f32)>),
    /// CPU-only or undetermined placement: contributes nowhere.
    None,
}

/// Attribute per-backend rates to GPUs. Returns one entry per GPU
/// that at least one backend contributed to; a GPU with no
/// contributor is absent from the map (its throughput for the tick
/// is unmeasured, not zero).
pub fn attribute_throughput(rates: &[(f32, GpuPlacement)]) -> HashMap<u32, f32> {
    let mut out: HashMap<u32, f32> = HashMap::new();
    for (rate, placement) in rates {
        match placement {
            GpuPlacement::Single(idx) => *out.entry(*idx).or_insert(0.0) += rate,
            GpuPlacement::Split(shares) => {
                for (idx, share) in shares {
                    *out.entry(*idx).or_insert(0.0) += rate * share;
                }
            }
            GpuPlacement::None => {}
        }
    }
    out
}

/// Average a non-empty run of points into one: temperature and
/// utilization average over every point; throughput and power
/// average over the points that measured them (absent when none
/// did). The result carries the FIRST point's timestamp — every
/// returned timestamp is a real sample's, never an invented one.
fn average_points(points: &[HistoryPoint]) -> HistoryPoint {
    debug_assert!(!points.is_empty());
    let n = points.len() as f32;
    let mean = |vals: &[f32]| vals.iter().sum::<f32>() / vals.len() as f32;
    let opt_mean = |get: &dyn Fn(&HistoryPoint) -> Option<f32>| {
        let vals: Vec<f32> = points.iter().filter_map(get).collect();
        if vals.is_empty() {
            None
        } else {
            Some(mean(&vals))
        }
    };
    HistoryPoint {
        ts_ms: points[0].ts_ms,
        temp_c: mean(&points.iter().map(|p| p.temp_c).collect::<Vec<_>>()),
        util_pct: mean(&points.iter().map(|p| p.util_pct).collect::<Vec<_>>()),
        throughput_toks: opt_mean(&|p: &HistoryPoint| p.throughput_toks),
        power_w: opt_mean(&|p: &HistoryPoint| p.power_w),
    }
}

/// Fold points into fixed-width time buckets (bucket-start
/// timestamps), averaging each bucket. Input need not be sorted;
/// output is oldest first. Used for the recent tier's contribution
/// to 7-day queries, whose nominal spacing is the bucket width.
fn bucket_average(points: &[HistoryPoint], bucket_ms: i64) -> Vec<HistoryPoint> {
    let mut buckets: HashMap<i64, Vec<HistoryPoint>> = HashMap::new();
    for p in points {
        let start = p.ts_ms - p.ts_ms.rem_euclid(bucket_ms);
        buckets.entry(start).or_default().push(*p);
    }
    let mut starts: Vec<i64> = buckets.keys().copied().collect();
    starts.sort_unstable();
    starts
        .into_iter()
        .map(|start| {
            let mut pts = buckets.remove(&start).expect("key from same map");
            pts.sort_by_key(|p| p.ts_ms);
            let mut avg = average_points(&pts);
            avg.ts_ms = start;
            avg
        })
        .collect()
}

/// Decimate by averaging consecutive groups so the result holds at
/// most `max` points. Grouping consecutive points (rather than
/// time buckets) keeps every timestamp a real sample's and keeps
/// gaps proportionally visible.
fn decimate(points: Vec<HistoryPoint>, max: usize) -> (Vec<HistoryPoint>, usize) {
    if points.len() <= max {
        return (points, 1);
    }
    let group = points.len().div_ceil(max);
    let out: Vec<HistoryPoint> = points
        .chunks(group)
        .map(average_points)
        .collect();
    (out, group)
}

impl GpuHistoryStore {
    /// Append one tick: a point per GPU from the fresh samples,
    /// with the attributed throughput map. Samples flagged down, or
    /// older than one sample interval relative to `now_ms` (a
    /// failing source's last-known values), record nothing — an
    /// outage is a gap, not a flat line. Then compaction runs, so
    /// the retention caps hold after every tick.
    pub fn record(
        &mut self,
        now_ms: i64,
        samples: &[GpuSample],
        throughput: &HashMap<u32, f32>,
    ) {
        for sample in samples {
            if sample.down {
                continue;
            }
            let ts_ms = sample.ts.timestamp_millis();
            if now_ms - ts_ms > SAMPLE_INTERVAL_MS || ts_ms > now_ms {
                continue;
            }
            let series = self.gpus.entry(sample.index).or_default();
            series.name = sample.name.clone();
            let point = HistoryPoint {
                ts_ms,
                temp_c: sample.temp_c,
                util_pct: sample.utilization_pct,
                throughput_toks: throughput.get(&sample.index).copied(),
                power_w: sample.power_w,
            };
            match series.recent.last_mut() {
                Some(last) if last.ts_ms == ts_ms => *last = point,
                Some(last) if last.ts_ms > ts_ms => {}
                _ => series.recent.push(point),
            }
        }
        self.compact(now_ms);
    }

    /// Enforce the retention scheme relative to `now_ms`: fold
    /// recent points older than the recent window into archive
    /// buckets, prune archive points beyond the archive window,
    /// and apply the hard count caps as a backstop.
    pub fn compact(&mut self, now_ms: i64) {
        let recent_cutoff = now_ms - RECENT_RETENTION_MS;
        let archive_cutoff = now_ms - ARCHIVE_RETENTION_MS;
        for series in self.gpus.values_mut() {
            let split = series
                .recent
                .iter()
                .position(|p| p.ts_ms >= recent_cutoff)
                .unwrap_or(series.recent.len());
            if split > 0 {
                let aged: Vec<HistoryPoint> = series.recent.drain(..split).collect();
                let mut folded = bucket_average(&aged, ARCHIVE_BUCKET_MS);
                // Merge with the existing archive by bucket: an
                // existing bucket point and a folded one for the
                // same bucket average together (equal weight per
                // point is an approximation across the fold boundary
                // — both sides are themselves averages of ~20 raw
                // points per bucket, so the error is negligible).
                let mut by_bucket: HashMap<i64, Vec<HistoryPoint>> = HashMap::new();
                for p in series.archive.drain(..).chain(folded.drain(..)) {
                    by_bucket.entry(p.ts_ms).or_default().push(p);
                }
                let mut starts: Vec<i64> = by_bucket.keys().copied().collect();
                starts.sort_unstable();
                series.archive = starts
                    .into_iter()
                    .map(|start| {
                        let pts = by_bucket.remove(&start).expect("key from same map");
                        let mut avg = average_points(&pts);
                        avg.ts_ms = start;
                        avg
                    })
                    .collect();
            }
            series
                .archive
                .retain(|p| p.ts_ms >= archive_cutoff);
            if series.archive.len() > ARCHIVE_CAP {
                let excess = series.archive.len() - ARCHIVE_CAP;
                series.archive.drain(..excess);
            }
            if series.recent.len() > RECENT_CAP {
                let excess = series.recent.len() - RECENT_CAP;
                series.recent.drain(..excess);
            }
        }
    }

    /// One GPU's points for a range, decimated to at most
    /// [`MAX_QUERY_POINTS`], with the nominal spacing of the
    /// returned points (the UI breaks a series' line where the
    /// actual gap exceeds 3× this spacing). `range_ms` is one of
    /// the supported windows (1 h / 24 h / 7 d).
    pub fn query(&self, index: u32, range_ms: i64, now_ms: i64) -> Option<GpuHistorySeries> {
        let series = self.gpus.get(&index)?;
        let from = now_ms - range_ms;
        let (points, interval_ms) = if range_ms <= RECENT_RETENTION_MS {
            let pts: Vec<HistoryPoint> = series
                .recent
                .iter()
                .copied()
                .filter(|p| p.ts_ms >= from)
                .collect();
            let (pts, group) = decimate(pts, MAX_QUERY_POINTS);
            (pts, SAMPLE_INTERVAL_MS * group as i64)
        } else {
            // The 7-day window spans both tiers at the archive's
            // nominal spacing: archive points as stored, recent
            // points folded into archive-width buckets.
            let mut pts: Vec<HistoryPoint> = series
                .archive
                .iter()
                .copied()
                .filter(|p| p.ts_ms >= from)
                .collect();
            let recent: Vec<HistoryPoint> = series
                .recent
                .iter()
                .copied()
                .filter(|p| p.ts_ms >= from)
                .collect();
            pts.extend(bucket_average(&recent, ARCHIVE_BUCKET_MS));
            pts.sort_by_key(|p| p.ts_ms);
            pts.dedup_by_key(|p| p.ts_ms);
            let (pts, group) = decimate(pts, MAX_QUERY_POINTS);
            (pts, ARCHIVE_BUCKET_MS * group as i64)
        };
        Some(GpuHistorySeries {
            index,
            name: series.name.clone(),
            interval_ms,
            points,
        })
    }

    /// Every GPU's series for a range (index order), for the query op.
    pub fn query_all(&self, range_ms: i64, now_ms: i64) -> Vec<GpuHistorySeries> {
        let mut idxs: Vec<u32> = self.gpus.keys().copied().collect();
        idxs.sort_unstable();
        idxs.into_iter()
            .filter_map(|i| self.query(i, range_ms, now_ms))
            .collect()
    }
}

/// One GPU's query result: the points plus their nominal spacing.
#[derive(Debug, Clone, Serialize)]
pub struct GpuHistorySeries {
    pub index: u32,
    pub name: String,
    pub interval_ms: i64,
    pub points: Vec<HistoryPoint>,
}

/// The AppState cell: the store plus the last successful save time
/// (the persistence throttle).
pub struct GpuHistoryCell {
    pub store: GpuHistoryStore,
    pub last_saved_ms: i64,
}

fn history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gpu-history.json")
}

/// Load the history. A missing or corrupt file is an empty history,
/// never an error (the activation-history discipline: the record is
/// an aid, and its absence must not break the app). Compaction runs
/// against the load time so a long-off period prunes on first read.
pub fn load_history(data_dir: &Path) -> GpuHistoryStore {
    let mut store = match std::fs::read_to_string(history_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => GpuHistoryStore::default(),
    };
    store.compact(chrono::Utc::now().timestamp_millis());
    store
}

/// Persist the history (tmp-file + rename, so a failed write leaves
/// the previous file intact). Compact JSON: at the retention caps
/// the pretty form would roughly double the worst-case size.
pub fn save_history(data_dir: &Path, store: &GpuHistoryStore) -> Result<(), String> {
    let text = serde_json::to_string(store)
        .map_err(|e| format!("the GPU history is not serializable: {e}"))?;
    let path = history_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    Ok(())
}

/// Parse a query range token ("1h" / "24h" / "7d") to milliseconds.
pub fn parse_range(range: &str) -> Result<i64, ApiError> {
    match range {
        "1h" => Ok(3_600_000),
        "24h" => Ok(24 * 3_600_000),
        "7d" => Ok(ARCHIVE_RETENTION_MS),
        other => Err(ApiError::InvalidInput(format!(
            "unknown GPU history range {other:?} (expected \"1h\", \"24h\", or \"7d\")"
        ))),
    }
}

/// One recorder tick: sample the telemetry snapshot, attribute the
/// running backends' latest gateway rates to their GPUs, append the
/// points, and persist if the save throttle allows. Best-effort by
/// contract — every failure degrades to a skipped tick, never an
/// error surface. Driven by the desktop shell on a 15 s loop,
/// mirroring the gateway metrics poller.
pub async fn record_gpu_history_once(state: &AppState) {
    // Adoption first, for the same reason the telemetry op does it:
    // the running set below comes from the supervisor's live pids,
    // and an unadopted orphan would attribute to nothing.
    crate::adopt_orphan_backends(state).await;
    let telemetry = state.telemetry().await;
    let samples = {
        let guard = crate::lock_telemetry(&telemetry);
        guard.snapshot()
    };
    if samples.is_empty() {
        return;
    }
    let running: std::collections::HashSet<String> = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
            .into_iter()
            .filter(|s| s.pid.is_some())
            .map(|s| s.id)
            .collect()
    };
    let cfg = state.config.read().await.clone();
    let pairs: Vec<(String, u16)> = cfg
        .backends
        .iter()
        .map(|b| (b.id.clone(), b.port))
        .collect();
    let series = {
        let stats = state.gateway_stats.lock().await;
        stats.series_stats(&pairs)
    };
    let mut rates: Vec<(f32, GpuPlacement)> = Vec::new();
    if let Some(series) = series {
        for b in &cfg.backends {
            if !running.contains(&b.id) {
                continue;
            }
            let Some(rate) = series
                .get(&b.id)
                .and_then(|s| s.toks_per_sec.last())
                .copied()
            else {
                continue;
            };
            let placement = match crate::backend_gpu_visibility(b) {
                crate::GpuVisibility::CpuOnly | crate::GpuVisibility::Undetermined => {
                    GpuPlacement::None
                }
                crate::GpuVisibility::Gpus(idxs) if idxs.len() == 1 => {
                    GpuPlacement::Single(idxs[0])
                }
                crate::GpuVisibility::Gpus(idxs) => {
                    let shares: Vec<(u32, f32)> = idxs
                        .iter()
                        .map(|i| {
                            (
                                *i,
                                b.flags
                                    .tensor_split
                                    .get(*i as usize)
                                    .copied()
                                    .unwrap_or(0.0),
                            )
                        })
                        .collect();
                    let total: f32 = shares.iter().map(|(_, f)| f).sum();
                    if total > 0.0 {
                        GpuPlacement::Split(
                            shares
                                .into_iter()
                                .map(|(i, f)| (i, f / total))
                                .collect(),
                        )
                    } else {
                        GpuPlacement::None
                    }
                }
            };
            if placement != GpuPlacement::None {
                rates.push((rate as f32, placement));
            }
        }
    }
    let throughput = attribute_throughput(&rates);
    let now_ms = crate::now_ms() as i64;
    let cell = state.gpu_history().await;
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    guard.store.record(now_ms, &samples, &throughput);
    if now_ms - guard.last_saved_ms >= SAVE_INTERVAL_MS {
        guard.last_saved_ms = now_ms;
        let _ = save_history(state.data_dir(), &guard.store);
    }
}

/// GPU history for one range: per-GPU decimated points for the
/// history charts. Payload: `{range, generated_at_ms, gpus}` where
/// each entry is `{index, name, interval_ms, points}` and each point
/// is `{ts_ms, temp_c, util_pct, throughput_toks, power_w}` with
/// null for unmeasured values.
pub async fn op_get_gpu_history(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    range: String,
) -> ApiResult {
    let range_ms = parse_range(&range)?;
    let now_ms = crate::now_ms() as i64;
    let cell = state.gpu_history().await;
    let gpus = {
        let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        guard.store.query_all(range_ms, now_ms)
    };
    Ok(serde_json::json!({
        "range": range,
        "generated_at_ms": now_ms,
        "gpus": gpus,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_telemetry::TelemetrySourceKind;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gpu-history-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(index: u32, ts_ms: i64, temp_c: f32) -> GpuSample {
        let mut s = GpuSample::fresh(
            index,
            &format!("GPU-{index}"),
            50.0,
            4096,
            16384,
            temp_c,
            100.0,
            TelemetrySourceKind::Nvml,
        );
        s.ts = chrono::DateTime::from_timestamp_millis(ts_ms).expect("valid ts");
        s
    }

    fn point(ts_ms: i64, temp_c: f32) -> HistoryPoint {
        HistoryPoint {
            ts_ms,
            temp_c,
            util_pct: 50.0,
            throughput_toks: None,
            power_w: Some(100.0),
        }
    }

    #[test]
    fn attribution_single_split_and_absent() {
        let rates = vec![
            (20.0, GpuPlacement::Single(0)),
            (30.0, GpuPlacement::Split(vec![(0, 0.5), (1, 0.5)])),
            (99.0, GpuPlacement::None),
        ];
        let out = attribute_throughput(&rates);
        assert_eq!(out.get(&0), Some(&35.0));
        assert_eq!(out.get(&1), Some(&15.0));
        assert_eq!(out.len(), 2, "the unplaced backend contributes nowhere");
        // No contributors at all: the map is empty, so every GPU's
        // throughput is absent for the tick — never a fabricated 0.
        assert!(attribute_throughput(&[]).is_empty());
    }

    #[test]
    fn record_appends_and_survives_restart() {
        let dir = test_dir("round-trip");
        let mut store = GpuHistoryStore::default();
        let t0 = 1_700_000_000_000;
        let mut throughput = HashMap::new();
        throughput.insert(0, 22.5);
        store.record(t0, &[sample(0, t0, 60.0)], &throughput);
        store.record(
            t0 + SAMPLE_INTERVAL_MS,
            &[sample(0, t0 + SAMPLE_INTERVAL_MS, 61.0)],
            &HashMap::new(),
        );
        save_history(&dir, &store).expect("saved");
        let loaded = load_history(&dir);
        let series = loaded.query(0, 3_600_000, t0 + SAMPLE_INTERVAL_MS).expect("series");
        assert_eq!(series.points.len(), 2);
        assert_eq!(series.points[0].throughput_toks, Some(22.5));
        assert_eq!(
            series.points[1].throughput_toks, None,
            "a tick with no contributor records absent throughput, not zero"
        );
        assert_eq!(series.points[0].power_w, Some(100.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_skips_down_and_stale_samples() {
        let mut store = GpuHistoryStore::default();
        let t0 = 1_700_000_000_000;
        let mut down = sample(0, t0, 60.0);
        down.down = true;
        store.record(t0, &[down], &HashMap::new());
        assert!(
            store.query(0, 3_600_000, t0).is_none_or(|s| s.points.is_empty()),
            "a down GPU records no point"
        );
        // A sample older than one interval is a failing source's
        // last-known value: recording it would flat-line the chart.
        store.record(t0, &[sample(1, t0 - 60_000, 55.0)], &HashMap::new());
        assert!(
            store.query(1, 3_600_000, t0).is_none_or(|s| s.points.is_empty()),
            "a stale sample records no point"
        );
    }

    #[test]
    fn power_absent_stays_absent() {
        let mut store = GpuHistoryStore::default();
        let t0 = 1_700_000_000_000;
        let mut s = sample(0, t0, 60.0);
        s.power_w = None; // driver answered [N/A]
        store.record(t0, &[s], &HashMap::new());
        let series = store.query(0, 3_600_000, t0).expect("series");
        assert_eq!(series.points[0].power_w, None);
    }

    #[test]
    fn retention_folds_recent_into_archive_and_prunes() {
        let mut store = GpuHistoryStore::default();
        // Eight days of 5-minute-spaced points fed straight into
        // the recent tier (record() would space them at 15 s; the
        // fold/prune math under test is in compact()).
        let start = 1_700_000_000_000;
        let mut series = GpuSeries {
            name: "GPU-0".to_string(),
            recent: (0..(8 * 24 * 12))
                .map(|i| point(start + i as i64 * ARCHIVE_BUCKET_MS, 60.0))
                .collect(),
            archive: Vec::new(),
        };
        let now = start + 8 * 24 * 3_600_000;
        store.gpus.insert(0, {
            series.recent.retain(|p| p.ts_ms <= now);
            series
        });
        store.compact(now);
        let series = &store.gpus[&0];
        // Recent keeps only the last 48 h.
        assert!(series
            .recent
            .iter()
            .all(|p| p.ts_ms >= now - RECENT_RETENTION_MS));
        assert!(!series.recent.is_empty());
        // Archive covers (48 h, 7 d] and nothing older than 7 d.
        assert!(series
            .archive
            .iter()
            .all(|p| p.ts_ms >= now - ARCHIVE_RETENTION_MS));
        assert!(!series.archive.is_empty());
        let oldest = series.archive.first().expect("archive points").ts_ms;
        assert!(
            oldest <= now - RECENT_RETENTION_MS,
            "archive reaches back past the recent window"
        );
        assert!(series.recent.len() <= RECENT_CAP);
        assert!(series.archive.len() <= ARCHIVE_CAP);
    }

    #[test]
    fn caps_hold_under_sustained_recording() {
        // Simulate 8 days at the real 15 s cadence in bulk: feed
        // compact() a recent tier built day by day so the test stays
        // fast while exercising the same fold path.
        let mut store = GpuHistoryStore::default();
        let day_ms = 24 * 3_600_000;
        let start = 1_700_000_000_000;
        for day in 0..8 {
            let day_start = start + day * day_ms;
            let mut series = store.gpus.entry(0).or_default();
            series.name = "GPU-0".to_string();
            for i in 0..(day_ms / SAMPLE_INTERVAL_MS) {
                series
                    .recent
                    .push(point(day_start + i * SAMPLE_INTERVAL_MS, 60.0));
            }
            store.compact(day_start + day_ms);
            let series = &store.gpus[&0];
            assert!(series.recent.len() <= RECENT_CAP, "day {day} recent cap");
            assert!(series.archive.len() <= ARCHIVE_CAP, "day {day} archive cap");
            // Worst-case size accounting: points are capped, so the
            // serialized file is bounded (~60 B/point + name).
            let text = serde_json::to_string(&store).unwrap();
            assert!(
                text.len() < 1_000_000,
                "day {day}: one GPU's history stays under 1 MB (got {})",
                text.len()
            );
        }
    }

    #[test]
    fn downsampling_averages_and_keeps_real_timestamps() {
        let pts: Vec<HistoryPoint> = (0..1200)
            .map(|i| {
                let mut p = point(i as i64 * SAMPLE_INTERVAL_MS, i as f32);
                p.throughput_toks = Some(10.0);
                p
            })
            .collect();
        let (out, group) = decimate(pts, MAX_QUERY_POINTS);
        assert_eq!(group, 2);
        assert_eq!(out.len(), 600);
        // Group averages: temps (0,1) -> 0.5 stamped with the first
        // point's real timestamp.
        assert_eq!(out[0].ts_ms, 0);
        assert!((out[0].temp_c - 0.5).abs() < f32::EPSILON);
        assert_eq!(out[0].throughput_toks, Some(10.0));
    }

    #[test]
    fn averages_skip_absent_optional_values() {
        let mut a = point(0, 60.0);
        a.power_w = None;
        let mut b = point(SAMPLE_INTERVAL_MS, 62.0);
        b.power_w = Some(120.0);
        let avg = average_points(&[a, b]);
        assert_eq!(avg.power_w, Some(120.0), "average over measured values only");
        let mut c = point(0, 60.0);
        c.power_w = None;
        let avg = average_points(&[c, c]);
        assert_eq!(avg.power_w, None, "absent in every input stays absent");
    }

    #[test]
    fn query_windows_and_gap_spacing() {
        let mut store = GpuHistoryStore::default();
        let t0 = 1_700_000_000_000;
        // Two clusters an hour apart (an app-off gap), 15 s apart
        // inside each cluster.
        for base in [t0, t0 + 3_600_000] {
            for i in 0..4 {
                store.record(
                    base + i * SAMPLE_INTERVAL_MS,
                    &[sample(0, base + i * SAMPLE_INTERVAL_MS, 60.0)],
                    &HashMap::new(),
                );
            }
        }
        let now = t0 + 3_600_000 + 45_000;
        let series = store.query(0, 3_600_000 * 2, now).expect("series");
        assert_eq!(series.points.len(), 8);
        assert_eq!(series.interval_ms, SAMPLE_INTERVAL_MS);
        let gap = series.points[4].ts_ms - series.points[3].ts_ms;
        assert!(
            gap > 3 * series.interval_ms,
            "the app-off hour is a detectable gap (gap {gap} ms)"
        );
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = test_dir("corrupt");
        std::fs::write(history_path(&dir), "{ not json").unwrap();
        assert!(load_history(&dir).gpus.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("1h").unwrap(), 3_600_000);
        assert_eq!(parse_range("24h").unwrap(), 86_400_000);
        assert_eq!(parse_range("7d").unwrap(), ARCHIVE_RETENTION_MS);
        assert!(parse_range("30d").is_err());
    }
}
