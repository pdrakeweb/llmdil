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

use serde::Serialize;
use tokio::sync::watch;

use crate::record::{audit_level, classify_level, ShipLevel, WireRecord, WIRE_VERSION};
use crate::redact::Redactor;
use crate::spool::{Spool, DEFAULT_SPOOL_CAP_BYTES};
use crate::tail::FileTail;
use crate::tap::RecordTap;

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
    /// S2: `gateway.stdout.log` / `gateway.stderr.log`, shipped as
    /// source `gateway` with the file's stream.
    pub tail_gateway: bool,
    /// S6: `service-manage.log`, the elevated service helper's own
    /// outcome log (`llm-service-host`, under `<data_dir>/logs`).
    pub tail_service_manage: bool,
    /// S7: `admin-api-audit.jsonl`. Lines are JSON records and ship
    /// verbatim (through redaction); the level comes from the
    /// record's `result`.
    pub tail_audit: bool,
    /// S5: app / supervisor events, offered on [`Self::tap`] as
    /// source `app`.
    pub app_events: bool,
    /// The in-process record feed. The shipper subscribes only when
    /// a tap-fed source is enabled, and accepts only records whose
    /// source is enabled.
    pub tap: Option<RecordTap>,
    /// S1 (opt-in, D3): per-backend output as source `backend:<id>`.
    /// `None` is off. `Some(ids)` tails each listed backend's
    /// service-mode files (`backend-<id>.stdout.log` / `.stderr.log`)
    /// and accepts `backend:*` records on the tap (process mode).
    pub backends: Option<Vec<String>>,
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
            tail_gateway: true,
            tail_service_manage: true,
            tail_audit: true,
            app_events: true,
            tap: None,
            backends: None,
            literal_secrets: Vec::new(),
            spool_cap_bytes: DEFAULT_SPOOL_CAP_BYTES,
        }
    }
}

/// What one source is doing right now (the per-source rows of the
/// status surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    /// Tailed or subscribed, and its input exists.
    Active,
    /// Toggled off: not tailed, not subscribed.
    Disabled,
    /// Enabled, but none of its log files exist (yet).
    FileMissing,
    /// Enabled and subscribed, with nothing captured yet: a backend
    /// that has no service log file and has not produced output in
    /// this process since the shipper started.
    Waiting,
    /// Enabled, but this process gave the shipper no in-process
    /// feed to subscribe to.
    Unavailable,
}

/// One source's row in [`ShipperStatus::sources`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceStatus {
    pub source: String,
    pub state: SourceState,
    /// Records this source has put on the queue since the shipper
    /// started (after the level floor, before delivery).
    pub records: u64,
}

/// One file a source tails.
struct TailFile {
    /// Spool-state key for the offset and fingerprint.
    key: String,
    stream: &'static str,
    path: PathBuf,
}

/// One source the shipper knows about, enabled or not — the plan
/// the taps are built from and the status rows are reported from,
/// so a toggle that does nothing cannot exist.
struct PlannedSource {
    source: String,
    enabled: bool,
    files: Vec<TailFile>,
    /// Fed by the in-process tap (instead of, or as well as, files).
    tapped: bool,
}

/// Source name of app / supervisor events on the tap (S5).
pub const APP_SOURCE: &str = "app";

/// Prefix of a per-backend source name (S1): `backend:<id>`.
pub const BACKEND_SOURCE_PREFIX: &str = "backend:";

/// The source name of one backend's output.
pub fn backend_source(id: &str) -> String {
    format!("{BACKEND_SOURCE_PREFIX}{id}")
}

/// Whether a tap record from `source` is one this config ships.
fn tap_accepts(config: &ShipperConfig, source: &str) -> bool {
    (source == APP_SOURCE && config.app_events)
        || (source.starts_with(BACKEND_SOURCE_PREFIX) && config.backends.is_some())
}

/// Every source for `config`, in report order.
fn source_plan(config: &ShipperConfig) -> Vec<PlannedSource> {
    let logs = config.data_dir.join("logs");
    let single = |source: &str, enabled: bool, file: &str| PlannedSource {
        source: source.to_string(),
        enabled,
        files: vec![TailFile {
            key: source.to_string(),
            stream: "stdout",
            path: logs.join(file),
        }],
        tapped: false,
    };
    let pair = |source: &str, enabled: bool, stem: &str| PlannedSource {
        source: source.to_string(),
        enabled,
        files: ["stdout", "stderr"]
            .into_iter()
            .map(|stream| TailFile {
                key: format!("{source}.{stream}"),
                stream,
                path: logs.join(format!("{stem}.{stream}.log")),
            })
            .collect(),
        tapped: false,
    };
    let mut plan = vec![
        single("wizard", config.tail_wizard, "wizard.log"),
        single("optimize", config.tail_optimize, "optimize.log"),
        pair("gateway", config.tail_gateway, "gateway"),
        single(
            "service-manage",
            config.tail_service_manage,
            "service-manage.log",
        ),
        single(AUDIT_SOURCE, config.tail_audit, "admin-api-audit.jsonl"),
        PlannedSource {
            source: APP_SOURCE.to_string(),
            enabled: config.app_events,
            files: Vec::new(),
            tapped: true,
        },
    ];
    // S1: one source per backend, fed both ways — the service host's
    // files (service mode) and the tap (process mode). Whichever
    // mode a backend runs in, only one of the two produces lines.
    match &config.backends {
        Some(ids) if !ids.is_empty() => {
            for id in ids {
                let mut src = pair(&backend_source(id), true, &format!("backend-{id}"));
                src.tapped = true;
                plan.push(src);
            }
        }
        other => plan.push(PlannedSource {
            source: "backends".to_string(),
            enabled: other.is_some(),
            files: Vec::new(),
            tapped: true,
        }),
    }
    plan
}

/// The rotation sibling the app's writers use: `<file>.prev`.
fn prev_sibling(path: &std::path::Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".prev");
    PathBuf::from(s)
}

/// Source name of the Admin API audit log tail (S7).
const AUDIT_SOURCE: &str = "audit";

/// The level of one line from `source`: audit records carry their
/// outcome, everything else is classified by the line heuristic.
fn source_level(source: &str, line: &str) -> ShipLevel {
    if source == AUDIT_SOURCE {
        audit_level(line)
    } else {
        classify_level(line)
    }
}

/// Redacts, sequences and enqueues records — the one path every
/// source's lines take into the queue, so redaction, the level
/// floor and the queue bound apply to all of them identically.
struct Intake {
    machine: String,
    app_version: String,
    level_floor: ShipLevel,
    redactor: Redactor,
    started: Instant,
    /// Records enqueued per source since start.
    counts: std::collections::BTreeMap<String, u64>,
}

impl Intake {
    #[allow(clippy::too_many_arguments)] // one record's fields
    fn admit(
        &mut self,
        spool: &mut Spool,
        status: &Arc<Mutex<ShipperStatus>>,
        queue: &mut VecDeque<WireRecord>,
        source: &str,
        stream: &str,
        level: ShipLevel,
        line: &str,
        // The shipper's own loss reports ship at every floor.
        bypass_floor: bool,
    ) {
        if !bypass_floor && level.rank() < self.level_floor.rank() {
            return;
        }
        let (line, counts) = self.redactor.redact(line);
        if counts.literal_lines > 0 || counts.pattern_hits > 0 {
            set_status(status, |s| {
                s.literal_redacted_lines += counts.literal_lines;
                s.pattern_redactions += counts.pattern_hits;
            });
        }
        let seq = spool.state.next_seq;
        spool.state.next_seq += 1;
        let record = WireRecord {
            v: WIRE_VERSION,
            machine: self.machine.clone(),
            app_version: self.app_version.clone(),
            seq,
            ts_wall: chrono::Local::now().to_rfc3339(),
            ts_mono_ms: self.started.elapsed().as_millis() as u64,
            source: source.to_string(),
            stream: stream.to_string(),
            level,
            line,
        };
        if queue.len() >= QUEUE_CAPACITY {
            // Full queue: drop the newest, count it (§6.4).
            set_status(status, |s| s.dropped_queue += 1);
        } else {
            queue.push_back(record);
            *self.counts.entry(source.to_string()).or_insert(0) += 1;
        }
    }
}

/// Label: Value status snapshot (design §6.5), surfaced by the app
/// on the Log receiver card's status payload.
#[derive(Debug, Clone, Default)]
pub struct ShipperStatus {
    /// "starting" | "connected" | "spooling"
    pub state: String,
    pub queued: u64,
    pub spooled_bytes: u64,
    pub delivered: u64,
    pub dropped_queue: u64,
    pub dropped_spool: u64,
    /// In-process records lost because the shipper fell behind the
    /// feed (each episode is also shipped as a warning record).
    pub dropped_events: u64,
    pub literal_redacted_lines: u64,
    pub pattern_redactions: u64,
    pub last_delivered: Option<String>,
    /// One row per known source, enabled or not.
    pub sources: Vec<SourceStatus>,
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

    /// Whether [`Self::shutdown`] has been requested (tasks feeding
    /// the shipper stop with it).
    pub fn is_shut_down(&self) -> bool {
        *self.shutdown.borrow()
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
    let mut intake = Intake {
        machine: config.machine.clone(),
        app_version: config.app_version.clone(),
        level_floor: config.level_floor,
        redactor: Redactor::new(config.literal_secrets.iter().map(|s| s.as_str())),
        started: Instant::now(),
        counts: Default::default(),
    };
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

    // A disabled source gets no tail at all — it is only reported.
    let plan = source_plan(&config);
    let mut tails: Vec<FileTail> = Vec::new();
    for src in plan.iter().filter(|s| s.enabled) {
        for file in &src.files {
            tails.push(
                FileTail::new(
                    &src.source,
                    file.path.clone(),
                    prev_sibling(&file.path),
                    spool.state.offsets.get(&file.key).copied(),
                )
                .with_stream(&file.key, file.stream)
                .with_fingerprint(spool.state.fingerprints.get(&file.key).copied()),
            );
        }
    }

    // Subscribe to the in-process feed only when a tap-fed source
    // is enabled: a disabled source is not subscribed.
    let mut tap_rx = config
        .tap
        .as_ref()
        .filter(|_| plan.iter().any(|s| s.tapped && s.enabled))
        .map(|tap| tap.subscribe());

    let started = intake.started;
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
            let lines = tail.poll();
            if let Some(o) = tail.offset {
                if spool.state.offsets.get(&tail.key) != Some(&o) {
                    spool.state.offsets.insert(tail.key.clone(), o);
                    dirty_offsets = true;
                }
                match tail.fingerprint() {
                    Some(fp) => spool.state.fingerprints.insert(tail.key.clone(), fp),
                    None => spool.state.fingerprints.remove(&tail.key),
                };
            }
            for line in lines {
                let level = source_level(&tail.source, &line);
                intake.admit(
                    &mut spool,
                    &status,
                    &mut queue,
                    &tail.source,
                    &tail.stream,
                    level,
                    &line,
                    false,
                );
            }
        }
        if dirty_offsets {
            let _ = spool.save_state();
        }

        // --- In-process feed: drain what is buffered, at most one
        // queue's worth per pass. `try_recv` never waits, and the
        // senders never wait on us: falling behind surfaces here
        // as `Lagged(n)`, shipped as one warning record.
        if let Some(rx) = tap_rx.as_mut() {
            use tokio::sync::broadcast::error::TryRecvError;
            for _ in 0..QUEUE_CAPACITY {
                match rx.try_recv() {
                    Ok(rec) => {
                        if !tap_accepts(&config, &rec.source) {
                            continue;
                        }
                        let level = rec
                            .level
                            .unwrap_or_else(|| source_level(&rec.source, &rec.line));
                        intake.admit(
                            &mut spool,
                            &status,
                            &mut queue,
                            &rec.source,
                            &rec.stream,
                            level,
                            &rec.line,
                            false,
                        );
                    }
                    Err(TryRecvError::Lagged(n)) => {
                        set_status(&status, |s| s.dropped_events += n);
                        intake.admit(
                            &mut spool,
                            &status,
                            &mut queue,
                            "shipper",
                            "stdout",
                            ShipLevel::Warn,
                            &format!(
                                "shipper: {n} in-process record(s) were dropped because the shipper fell behind the event feed"
                            ),
                            true,
                        );
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                }
            }
        }

        // --- Per-source status rows. -----------------------------
        let mut rows: Vec<SourceStatus> = plan
            .iter()
            .map(|src| {
                let records = intake.counts.get(&src.source).copied().unwrap_or(0);
                let subscribed = src.tapped && tap_rx.is_some();
                let state = if !src.enabled {
                    SourceState::Disabled
                } else if tails.iter().any(|t| t.source == src.source && t.present) {
                    SourceState::Active
                } else if src.files.is_empty() {
                    // Fed by the tap alone.
                    if subscribed {
                        SourceState::Active
                    } else {
                        SourceState::Unavailable
                    }
                } else if !subscribed {
                    SourceState::FileMissing
                } else if records > 0 {
                    SourceState::Active
                } else {
                    SourceState::Waiting
                };
                SourceStatus {
                    source: src.source.clone(),
                    state,
                    records,
                }
            })
            .collect();
        // Backends that appeared after the plan was built (added to
        // the config since the shipper started) and are shipping.
        for (source, records) in &intake.counts {
            if source.starts_with(BACKEND_SOURCE_PREFIX)
                && !rows.iter().any(|r| &r.source == source)
            {
                rows.push(SourceStatus {
                    source: source.clone(),
                    state: SourceState::Active,
                    records: *records,
                });
            }
        }
        set_status(&status, |s| s.sources = rows);

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
