//! The shipper task (design §6.1, §6.4, §6.6).
//!
//! One background tokio task inside the LLMdil process: polls the
//! file tails, redacts and sequences records into a bounded queue
//! (capacity 1,024 — a full queue drops the *newest* record and
//! counts it; logging never blocks), batches (100 records or 2 s),
//! and POSTs gzipped batches to the single configured destination.
//! Undeliverable batches spool to disk and drain in order on
//! recovery before new batches send. The task is supervised but
//! never supervising: a panic is caught at the task boundary,
//! written to a local `shipper.log`, and the task restarts with
//! backoff. Shipper errors are logged locally only — `shipper.log`
//! is never tailed, so there is no recursion.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::record::{classify_level, ShipLevel, WireRecord, WIRE_VERSION};
use crate::redact::Redactor;
use crate::spool::{Spool, DEFAULT_SPOOL_CAP_BYTES};
use crate::tail::FileTail;

/// Bounded queue capacity in records (design D9).
pub const QUEUE_CAPACITY: usize = 1024;
/// Batch flush threshold in records (design §6.4).
pub const BATCH_MAX: usize = 100;
/// Batch flush window (design §6.4).
pub const BATCH_WINDOW: Duration = Duration::from_secs(2);
/// Tail poll cadence.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct ShipperConfig {
    pub data_dir: PathBuf,
    /// Destination base URL; the shipper appends `/v1/batches`.
    pub endpoint: String,
    /// Ingest token (Credential Manager value, D6).
    pub token: String,
    pub machine: String,
    pub app_version: String,
    pub level_floor: ShipLevel,
    pub tail_wizard: bool,
    pub tail_optimize: bool,
    /// Literal secret values for redaction layer 1.
    pub literal_secrets: Vec<String>,
    pub spool_cap_bytes: u64,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::new(),
            endpoint: String::new(),
            token: String::new(),
            machine: String::new(),
            app_version: String::new(),
            level_floor: ShipLevel::Info,
            tail_wizard: true,
            tail_optimize: true,
            literal_secrets: Vec::new(),
            spool_cap_bytes: DEFAULT_SPOOL_CAP_BYTES,
        }
    }
}

/// Label: Value status snapshot (design §6.5), read by the app for
/// the `log_streaming_status` surface in Phase 2; maintained from
/// the first build so the counters exist when the surface lands.
#[derive(Debug, Clone, Default)]
pub struct ShipperStatus {
    /// "starting" | "connected" | "spooling"
    pub state: String,
    pub queued: u64,
    pub spooled_bytes: u64,
    pub delivered: u64,
    pub dropped_queue: u64,
    pub dropped_spool: u64,
    pub literal_redacted_lines: u64,
    pub pattern_redactions: u64,
    pub last_delivered: Option<String>,
}

pub struct ShipperHandle {
    status: Arc<Mutex<ShipperStatus>>,
    shutdown: watch::Sender<bool>,
}

impl ShipperHandle {
    pub fn status(&self) -> ShipperStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Spawn the supervised shipper task. Requires a tokio runtime
/// context (the app's).
pub fn spawn(config: ShipperConfig) -> ShipperHandle {
    let status = Arc::new(Mutex::new(ShipperStatus {
        state: "starting".to_string(),
        ..ShipperStatus::default()
    }));
    let (tx, rx) = watch::channel(false);
    let st = status.clone();
    tokio::spawn(supervise(config, st, rx));
    ShipperHandle {
        status,
        shutdown: tx,
    }
}

async fn supervise(
    config: ShipperConfig,
    status: Arc<Mutex<ShipperStatus>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let cfg = config.clone();
        let st = status.clone();
        let rx = shutdown.clone();
        let join = tokio::spawn(run_loop(cfg, st, rx));
        match join.await {
            Ok(()) => return,
            Err(e) => {
                append_shipper_log(
                    &config.data_dir,
                    &format!(
                        "shipper task ended abnormally ({e}); restarting in 5 s"
                    ),
                );
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                    _ = shutdown.changed() => return,
                }
            }
        }
    }
}

/// Append one line to the shipper's own local log
/// (`<data_dir>/logs/shipper.log`) — local only, never shipped
/// (design §6.6, no recursion).
pub fn append_shipper_log(data_dir: &std::path::Path, line: &str) {
    let logs = data_dir.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return;
    }
    let ts = chrono::Local::now().to_rfc3339();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("shipper.log"))
    {
        let _ = writeln!(f, "[{ts}] {line}");
    }
}

fn set_status(status: &Arc<Mutex<ShipperStatus>>, f: impl FnOnce(&mut ShipperStatus)) {
    if let Ok(mut s) = status.lock() {
        f(&mut s);
    }
}

async fn run_loop(
    config: ShipperConfig,
    status: Arc<Mutex<ShipperStatus>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let logs_dir = config.data_dir.join("logs");
    let spool_dir = logs_dir.join("shipper-spool");
    let mut spool = match Spool::open(&spool_dir, config.spool_cap_bytes) {
        Ok(s) => s,
        Err(e) => {
            append_shipper_log(
                &config.data_dir,
                &format!("shipper cannot open spool {}: {e}", spool_dir.display()),
            );
            return;
        }
    };
    let redactor = Redactor::new(config.literal_secrets.iter().map(|s| s.as_str()));
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            append_shipper_log(&config.data_dir, &format!("shipper cannot build HTTP client: {e}"));
            return;
        }
    };
    let endpoint = format!("{}/v1/batches", config.endpoint.trim_end_matches('/'));

    let mut tails: Vec<FileTail> = Vec::new();
    if config.tail_wizard {
        tails.push(FileTail::new(
            "wizard",
            logs_dir.join("wizard.log"),
            logs_dir.join("wizard.log.prev"),
            spool.state.offsets.get("wizard").copied(),
        ));
    }
    if config.tail_optimize {
        tails.push(FileTail::new(
            "optimize",
            logs_dir.join("optimize.log"),
            logs_dir.join("optimize.log.prev"),
            spool.state.offsets.get("optimize").copied(),
        ));
    }

    let started = Instant::now();
    let mut queue: VecDeque<WireRecord> = VecDeque::new();
    let mut batch: Vec<WireRecord> = Vec::new();
    let mut batch_since: Option<Instant> = None;
    let mut failures: u32 = 0;
    let mut next_attempt = Instant::now();
    let mut episode_logged = false;

    loop {
        if *shutdown.borrow() {
            return;
        }

        // --- Taps: poll tails, redact, sequence, enqueue. --------
        let mut dirty_offsets = false;
        for tail in &mut tails {
            for line in tail.poll() {
                if let Some(o) = tail.offset {
                    spool.state.offsets.insert(tail.source.clone(), o);
                    dirty_offsets = true;
                }
                let level = classify_level(&line);
                if level.rank() < config.level_floor.rank() {
                    continue;
                }
                let (line, counts) = redactor.redact(&line);
                if counts.literal_lines > 0 || counts.pattern_hits > 0 {
                    set_status(&status, |s| {
                        s.literal_redacted_lines += counts.literal_lines;
                        s.pattern_redactions += counts.pattern_hits;
                    });
                }
                let seq = spool.state.next_seq;
                spool.state.next_seq += 1;
                let record = WireRecord {
                    v: WIRE_VERSION,
                    machine: config.machine.clone(),
                    app_version: config.app_version.clone(),
                    seq,
                    ts_wall: chrono::Local::now().to_rfc3339(),
                    ts_mono_ms: started.elapsed().as_millis() as u64,
                    source: tail.source.clone(),
                    stream: "stdout".to_string(),
                    level,
                    line,
                };
                if queue.len() >= QUEUE_CAPACITY {
                    // Full queue: drop the newest, count it (§6.4).
                    set_status(&status, |s| s.dropped_queue += 1);
                } else {
                    queue.push_back(record);
                }
            }
        }
        if dirty_offsets {
            let _ = spool.save_state();
        }

        // --- Batching: spool drains first, then the live queue. --
        let now = Instant::now();
        if batch.is_empty() {
            if let Ok(Some((_path, records))) = spool.oldest_segment() {
                batch = records;
                batch_since = Some(now);
            } else {
                while batch.len() < BATCH_MAX {
                    match queue.pop_front() {
                        Some(r) => {
                            if batch_since.is_none() {
                                batch_since = Some(now);
                            }
                            batch.push(r);
                        }
                        None => break,
                    }
                }
            }
        }
        let batch_ready = !batch.is_empty()
            && (batch.len() >= BATCH_MAX
                || batch_since.map(|t| now.duration_since(t) >= BATCH_WINDOW).unwrap_or(false)
                || spool.segment_count() > 0);
        if batch_ready && now >= next_attempt {
            // A spool segment sends as-is; a live batch is `batch`.
            let from_spool = spool.segment_count() > 0
                && spool
                    .oldest_segment()
                    .ok()
                    .flatten()
                    .map(|(_, recs)| !recs.is_empty() && recs[0].seq == batch[0].seq)
                    .unwrap_or(false);
            match post_batch(&client, &endpoint, &config.token, &batch).await {
                Ok(()) => {
                    let delivered = batch.len() as u64;
                    if from_spool {
                        if let Ok(Some((path, _))) = spool.oldest_segment() {
                            let _ = spool.remove_segment(&path);
                        }
                    }
                    batch.clear();
                    batch_since = None;
                    // Persist next_seq past everything just
                    // delivered: a crash must never reuse a sequence
                    // number the receiver already accepted (it
                    // dedupes on (machine, seq)).
                    let _ = spool.save_state();
                    if failures > 0 {
                        append_shipper_log(
                            &config.data_dir,
                            "shipper destination recovered; spool draining",
                        );
                    }
                    failures = 0;
                    episode_logged = false;
                    set_status(&status, |s| {
                        s.delivered += delivered;
                        s.last_delivered = Some(chrono::Local::now().to_rfc3339());
                        s.state = "connected".to_string();
                    });
                    // Report spool-cap drops as the next record out
                    // (§6.4: the loss is always itself reported).
                    if spool.state.unreported_spool_drops > 0 && spool.segment_count() == 0 {
                        let n = spool.state.unreported_spool_drops;
                        spool.state.unreported_spool_drops = 0;
                        let _ = spool.save_state();
                        let seq = spool.state.next_seq;
                        spool.state.next_seq += 1;
                        queue.push_front(WireRecord {
                            v: WIRE_VERSION,
                            machine: config.machine.clone(),
                            app_version: config.app_version.clone(),
                            seq,
                            ts_wall: chrono::Local::now().to_rfc3339(),
                            ts_mono_ms: started.elapsed().as_millis() as u64,
                            source: "shipper".to_string(),
                            stream: "stdout".to_string(),
                            level: ShipLevel::Warn,
                            line: format!(
                                "shipper: {n} spooled record(s) were dropped at the spool cap while the destination was unreachable"
                            ),
                        });
                    }
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    if !episode_logged {
                        append_shipper_log(
                            &config.data_dir,
                            &format!("shipper delivery failed ({e}); spooling"),
                        );
                        episode_logged = true;
                    }
                    if !from_spool {
                        // Live batch becomes a spool segment; a
                        // spool batch is already on disk.
                        match spool.append_batch(&batch) {
                            Ok(dropped) => {
                                if dropped > 0 {
                                    set_status(&status, |s| s.dropped_spool += dropped);
                                }
                            }
                            Err(se) => {
                                append_shipper_log(
                                    &config.data_dir,
                                    &format!("shipper spool append failed: {se}"),
                                );
                                set_status(&status, |s| {
                                    s.dropped_spool += batch.len() as u64
                                });
                            }
                        }
                    }
                    batch.clear();
                    batch_since = None;
                    // Persist next_seq past the spooled records
                    // for the same dedupe reason as on delivery.
                    let _ = spool.save_state();
                    let backoff = backoff_for(failures, spool.state.next_seq);
                    next_attempt = Instant::now() + backoff;
                    set_status(&status, |s| s.state = "spooling".to_string());
                }
            }
        }

        set_status(&status, |s| {
            s.queued = queue.len() as u64;
            s.spooled_bytes = spool.spooled_bytes();
        });

        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {},
            _ = shutdown.changed() => return,
        }
    }
}

/// Exponential backoff 5 s → 5 min cap (design §6.4) with a small
/// deterministic jitter derived from the sequence counter.
fn backoff_for(failures: u32, seq: u64) -> Duration {
    let shift = failures.saturating_sub(1).min(6);
    let base = Duration::from_secs(5u64 << shift).min(Duration::from_secs(300));
    base + Duration::from_millis((seq % 7) * 100)
}

/// Serialize + gzip one batch and POST it with the ingest token.
/// Success is any 2xx.
async fn post_batch(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    batch: &[WireRecord],
) -> Result<(), String> {
    let json = serde_json::to_vec(batch).map_err(|e| e.to_string())?;
    let gz = gzip(&json)?;
    let resp = client
        .post(endpoint)
        .header("authorization", format!("Bearer {token}"))
        .header("content-encoding", "gzip")
        .header("content-type", "application/json")
        .body(gz)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("destination returned {}", resp.status()))
    }
}

fn gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())
}
