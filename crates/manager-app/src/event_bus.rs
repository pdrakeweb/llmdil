//! The app-level event bus: one in-process stream of every UI event,
//! whichever surface caused it (spec §14 events; Admin API
//! `GET /api/v1/events`).
//!
//! Before this bus, operation events went only to the sink of the
//! caller that dispatched the operation: the desktop shell's Tauri
//! sink for IPC calls, the Admin API's broadcast sink for HTTP calls.
//! An action taken in the desktop UI therefore never reached an API
//! event subscriber. Now [`crate::registry::dispatch`] (desktop path)
//! and [`crate::registry::dispatch_from`] (API path) tee every event an
//! operation emits into this bus, tagged with an [`EventOrigin`], at
//! the one choke point both paths share — no operation had to change.
//!
//! Producers that are not operations publish here directly:
//!
//! - `task-progress` — a task record created or settled (the
//!   `tasks_list` row shape);
//! - `alert-raised` — a failure / activation transition reported by
//!   the notification monitor (whether or not a toast is allowed);
//! - `telemetry-tick`, `vram-changed`, `log-line` — the background
//!   producer loop ([`subscribe`] starts it; it stops itself when the
//!   last subscriber goes away), so nothing is polled for an audience
//!   that does not exist.
//!
//! Dispatch-level events (`config-changed`, `gateway-request`) are
//! emitted by [`after_dispatch`] through the operation's own sink, so
//! they reach every consumer an operation event reaches.
//!
//! Every payload is data a `read`-class caller can already GET
//! (`tasks_list`, `get_gpu_telemetry`, `get_logs`,
//! `gateway_recent_requests`, `list_backends`); `config-changed`
//! carries section *names* only, never values. The stream is the
//! `read`-class `GET /events`, so no per-role filtering is needed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::registry::{CapClass, Op};
use crate::{events, ApiError, AppState, EventSink, TaskRecord};

/// Bus buffer: a subscriber further behind than this loses the
/// oldest events (`RecvError::Lagged`), never blocks a producer.
pub const BUS_CAPACITY: usize = 1024;

/// How often the background producer loop wakes. Telemetry ticks
/// still follow the poller's own cadence (one per completed poll);
/// this only bounds how late a tick is observed.
const PRODUCER_TICK: Duration = Duration::from_millis(250);

/// Minimum spacing between `telemetry-tick` events (the poller runs
/// at 1 s or 5 s; this caps a faster future cadence).
pub const TELEMETRY_TICK_MIN_SPACING: Duration = Duration::from_secs(1);

/// How often captured process output is followed for `log-line`.
const LOG_FOLLOW_EVERY: Duration = Duration::from_millis(500);

/// Most `log-line` events per scope per follow pass; the rest are
/// summarized in one notice line (the full output stays in
/// `GET /logs`).
pub const LOG_LINES_PER_SCOPE_PER_PASS: usize = 50;

/// A VRAM move at least this large (MiB) is a `vram-changed` event...
pub const VRAM_CHANGE_MIN_MIB: u64 = 256;
/// ...or at least this share of the card's total, whichever is larger.
pub const VRAM_CHANGE_MIN_FRACTION: f64 = 0.02;

/// Which surface caused an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventOrigin {
    /// An operation invoked from the desktop UI (Tauri IPC).
    Desktop,
    /// An operation invoked through the Admin API.
    Api,
    /// Background work not attributable to a caller (pollers,
    /// monitors, watchers).
    App,
}

impl EventOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Api => "api",
            Self::App => "app",
        }
    }
}

/// One event on the bus.
#[derive(Debug, Clone)]
pub struct BusEvent {
    pub event: String,
    pub payload: Value,
    pub origin: EventOrigin,
}

/// The broadcast channel behind the bus, owned by [`AppState`].
pub struct AppEventBus {
    tx: broadcast::Sender<BusEvent>,
    /// The background producer loop is running.
    producers: AtomicBool,
}

impl Default for AppEventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            producers: AtomicBool::new(false),
        }
    }
}

impl std::fmt::Debug for AppEventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppEventBus")
            .field("subscribers", &self.tx.receiver_count())
            .finish()
    }
}

impl AppEventBus {
    /// Whether anyone is listening. Producers check this before
    /// building a payload.
    pub fn has_subscribers(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Publish one event; a no-op with no subscribers.
    pub fn publish(&self, event: &str, payload: Value, origin: EventOrigin) {
        if self.has_subscribers() {
            let _ = self.tx.send(BusEvent {
                event: event.to_string(),
                payload,
                origin,
            });
        }
    }
}

impl AppState {
    /// The app-level event bus (see [`crate::event_bus`]).
    pub fn event_bus(&self) -> &AppEventBus {
        &self.event_bus
    }
}

/// Subscribe to the bus and make sure the background producers
/// (`telemetry-tick`, `vram-changed`, `log-line`) are running. They
/// stop on their own once no subscriber is left.
pub fn subscribe(state: &Arc<AppState>) -> broadcast::Receiver<BusEvent> {
    let rx = state.event_bus.tx.subscribe();
    ensure_producers(state);
    rx
}

// ---------------------------------------------------------------------------
// Origin
// ---------------------------------------------------------------------------

tokio::task_local! {
    static ORIGIN: EventOrigin;
}

/// The origin of the dispatch this code runs inside, or
/// [`EventOrigin::App`] outside any dispatch (background tasks
/// spawned by an operation keep their origin through the sink they
/// captured, not through this).
pub fn current_origin() -> EventOrigin {
    ORIGIN.try_with(|o| *o).unwrap_or(EventOrigin::App)
}

/// Run `fut` with `origin` as the [`current_origin`].
pub(crate) async fn with_origin<F: std::future::Future>(origin: EventOrigin, fut: F) -> F::Output {
    ORIGIN.scope(origin, fut).await
}

// ---------------------------------------------------------------------------
// The operation sink tee
// ---------------------------------------------------------------------------

struct BusSink {
    inner: Arc<dyn EventSink>,
    tx: broadcast::Sender<BusEvent>,
    origin: EventOrigin,
}

impl EventSink for BusSink {
    fn emit(&self, event: &str, payload: Value) {
        if self.tx.receiver_count() > 0 {
            let _ = self.tx.send(BusEvent {
                event: event.to_string(),
                payload: payload.clone(),
                origin: self.origin,
            });
        }
        self.inner.emit(event, payload);
    }

    fn toast(&self, title: &str, body: &str) -> Result<(), String> {
        self.inner.toast(title, body)
    }
}

/// Wrap a caller's sink so every event also reaches the bus tagged
/// with `origin`. The tee holds a sender clone, so events from work
/// an operation spawns (download progress) keep their origin.
pub(crate) fn bus_sink(
    state: &AppState,
    inner: Arc<dyn EventSink>,
    origin: EventOrigin,
) -> Arc<dyn EventSink> {
    Arc::new(BusSink {
        inner,
        tx: state.event_bus.tx.clone(),
        origin,
    })
}

// ---------------------------------------------------------------------------
// Dispatch hooks: config-changed, gateway-request
// ---------------------------------------------------------------------------

/// What [`before_dispatch`] captured for [`after_dispatch`].
pub(crate) struct DispatchWatch {
    config_before: Option<Value>,
}

/// Classes whose operations can write configuration. Reads never do
/// (and are the hot polling path, so they pay nothing here).
fn may_write_config(class: CapClass) -> bool {
    !matches!(
        class,
        CapClass::R | CapClass::SConfig | CapClass::SDisclosure
    )
}

/// Snapshot the config before a possibly-mutating operation, only
/// when someone is listening.
pub(crate) async fn before_dispatch(state: &AppState, op: Op) -> DispatchWatch {
    let config_before =
        if state.event_bus.has_subscribers() && may_write_config(crate::registry::spec(op).class) {
            serde_json::to_value(&*state.config.read().await).ok()
        } else {
            None
        };
    DispatchWatch { config_before }
}

/// The top-level config sections whose value differs.
pub fn changed_sections(before: &Value, after: &Value) -> Vec<String> {
    let empty = serde_json::Map::new();
    let b = before.as_object().unwrap_or(&empty);
    let a = after.as_object().unwrap_or(&empty);
    let mut keys: Vec<String> = b
        .keys()
        .chain(a.keys())
        .filter(|k| b.get(*k) != a.get(*k))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Emit the dispatch-level events for a finished operation through
/// its sink. Semantics: only a *successful* operation emits;
/// `config-changed` additionally requires an observed difference in
/// the saved configuration (section names only).
pub(crate) async fn after_dispatch(
    state: &AppState,
    sink: &Arc<dyn EventSink>,
    op: Op,
    result: &Result<Value, ApiError>,
    watch: DispatchWatch,
) {
    let Ok(value) = result else {
        return;
    };
    if op == Op::GatewayTestRouting {
        // The decision the op just recorded in the routing ring
        // (`gateway_recent_requests` row shape).
        sink.emit(events::GATEWAY_REQUEST, value.clone());
    }
    if let Some(before) = watch.config_before {
        let after = serde_json::to_value(&*state.config.read().await).unwrap_or(Value::Null);
        let sections = changed_sections(&before, &after);
        if !sections.is_empty() {
            sink.emit(
                events::CONFIG_CHANGED,
                json!({
                    "op": crate::registry::spec(op).name,
                    "sections": sections,
                    "at_ms": crate::now_ms(),
                }),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Direct producers: task-progress, alert-raised
// ---------------------------------------------------------------------------

/// A task record was created or settled.
pub(crate) fn publish_task(state: &AppState, record: &TaskRecord) {
    if state.event_bus.has_subscribers() {
        if let Ok(v) = serde_json::to_value(record) {
            state
                .event_bus
                .publish(events::TASK_PROGRESS, v, current_origin());
        }
    }
}

/// A failure or activation transition the notification monitor
/// reported.
pub(crate) fn publish_alert(state: &AppState, category: &str, title: &str, body: &str) {
    state.event_bus.publish(
        events::ALERT_RAISED,
        json!({
            "category": category,
            "title": title,
            "body": body,
            "at_ms": crate::now_ms(),
        }),
        current_origin(),
    );
}

// ---------------------------------------------------------------------------
// Background producers: telemetry-tick, vram-changed, log-line
// ---------------------------------------------------------------------------

fn ensure_producers(state: &Arc<AppState>) {
    let bus = &state.event_bus;
    if bus.producers.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        bus.producers.store(false, Ordering::SeqCst);
        return;
    };
    let state = Arc::clone(state);
    handle.spawn(async move { producer_loop(state).await });
}

async fn producer_loop(state: Arc<AppState>) {
    let mut telemetry = TelemetryWatch::default();
    let mut logs = LogFollow::default();
    let mut ticker = tokio::time::interval(PRODUCER_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let bus = &state.event_bus;
        if !bus.has_subscribers() {
            bus.producers.store(false, Ordering::SeqCst);
            // A subscriber that arrived between the check and the
            // store found the flag still set and spawned nothing:
            // take the loop back over for it.
            if bus.has_subscribers() && !bus.producers.swap(true, Ordering::SeqCst) {
                continue;
            }
            return;
        }
        telemetry.step(&state);
        if logs.due() {
            logs.step(&state).await;
        }
    }
}

/// Follows the telemetry store (it is only observed, never started:
/// the poller starts on first telemetry use, as before).
#[derive(Default)]
struct TelemetryWatch {
    last_poll: Option<chrono::DateTime<chrono::Utc>>,
    last_tick: Option<Instant>,
    vram_baseline: HashMap<u32, u64>,
}

impl TelemetryWatch {
    fn step(&mut self, state: &AppState) {
        let Some(store) = state.telemetry.get() else {
            return;
        };
        let (samples, polled) = {
            let guard = crate::lock_telemetry(store);
            (guard.snapshot(), guard.last_successful_poll_at())
        };
        let Some(polled) = polled else {
            return;
        };
        if self.last_poll == Some(polled) {
            return;
        }
        self.last_poll = Some(polled);
        let bus = &state.event_bus;
        for change in vram_changes(&mut self.vram_baseline, &samples) {
            bus.publish(events::VRAM_CHANGED, change, EventOrigin::App);
        }
        let due = self
            .last_tick
            .is_none_or(|t| t.elapsed() >= TELEMETRY_TICK_MIN_SPACING);
        if due {
            self.last_tick = Some(Instant::now());
            bus.publish(
                events::TELEMETRY_TICK,
                json!({
                    "samples": samples,
                    "at_ms": polled.timestamp_millis(),
                }),
                EventOrigin::App,
            );
        }
    }
}

/// The `vram-changed` payloads for one poll. The first observation
/// of a GPU sets its baseline silently; afterwards a move of at
/// least max([`VRAM_CHANGE_MIN_MIB`], [`VRAM_CHANGE_MIN_FRACTION`] of
/// the card) from the last *reported* level is an event (so a slow
/// drift still reports once it adds up). Down GPUs are skipped.
pub fn vram_changes(
    baseline: &mut HashMap<u32, u64>,
    samples: &[manager_telemetry::GpuSample],
) -> Vec<Value> {
    let mut out = Vec::new();
    for s in samples.iter().filter(|s| !s.down) {
        let Some(&prev) = baseline.get(&s.index) else {
            baseline.insert(s.index, s.vram_used_mib);
            continue;
        };
        let threshold = VRAM_CHANGE_MIN_MIB
            .max((s.vram_total_mib as f64 * VRAM_CHANGE_MIN_FRACTION).round() as u64);
        let delta = s.vram_used_mib as i64 - prev as i64;
        if delta.unsigned_abs() >= threshold {
            baseline.insert(s.index, s.vram_used_mib);
            out.push(json!({
                "index": s.index,
                "name": s.name,
                "vram_used_mib": s.vram_used_mib,
                "vram_total_mib": s.vram_total_mib,
                "previous_mib": prev,
                "delta_mib": delta,
                "at_ms": s.ts.timestamp_millis(),
            }));
        }
    }
    out
}

/// Follows every supervisor log ring (the rings `get_logs` reads).
#[derive(Default)]
struct LogFollow {
    last: Option<Instant>,
    /// Per-scope cursor; a scope present at the first pass starts at
    /// its current end (no backlog dump), a scope that appears later
    /// streams from its first line.
    cursors: HashMap<String, Option<manager_supervisor::LogCursor>>,
    primed: bool,
}

impl LogFollow {
    fn due(&mut self) -> bool {
        if self.last.is_none_or(|t| t.elapsed() >= LOG_FOLLOW_EVERY) {
            self.last = Some(Instant::now());
            true
        } else {
            false
        }
    }

    async fn step(&mut self, state: &AppState) {
        // Observe only: never initialize the supervisor for a
        // subscriber, and never wait behind a lifecycle operation
        // holding it (the next pass catches up).
        let Some(sup) = state.supervisor.get() else {
            return;
        };
        let Ok(sup) = sup.try_lock() else {
            return;
        };
        let mut lines: Vec<Value> = Vec::new();
        for scope in sup.log_scopes() {
            let id = scope.id;
            let known = self.cursors.get(&id).copied();
            let (records, next) = sup.logs_since(&id, known.flatten());
            self.cursors.insert(id.clone(), next);
            if known.is_none() && !self.primed {
                continue;
            }
            lines.extend(log_line_payloads(&id, &records));
        }
        self.primed = true;
        drop(sup);
        for line in lines {
            state
                .event_bus
                .publish(events::LOG_LINE, line, EventOrigin::App);
        }
    }
}

/// The level a captured line is reported at: the supervisor's own
/// `get_logs` level heuristic, most severe match first.
fn line_level(line: &str) -> &'static str {
    use manager_supervisor::LogLevelFilter as L;
    if L::Error.matches(line) {
        "error"
    } else if L::Warn.matches(line) {
        "warn"
    } else if L::Debug.matches(line) {
        "debug"
    } else {
        "info"
    }
}

/// `log-line` payloads for one scope's new records (the
/// `LogLinePayload` fields plus `stream`), capped at
/// [`LOG_LINES_PER_SCOPE_PER_PASS`]: the newest are kept and one
/// notice line counts the skipped ones.
pub fn log_line_payloads(scope: &str, records: &[manager_supervisor::LogRecord]) -> Vec<Value> {
    let skipped = records.len().saturating_sub(LOG_LINES_PER_SCOPE_PER_PASS);
    let mut out = Vec::new();
    if skipped > 0 {
        out.push(json!({
            "scope": scope,
            "level": "warn",
            "line": format!(
                "[llmdil] {skipped} line(s) not streamed (rate limit); GET /api/v1/logs has them"
            ),
            "ts_ms": records[skipped - 1].ts.timestamp_millis(),
            "skipped": skipped,
        }));
    }
    for r in &records[skipped..] {
        let payload = crate::LogLinePayload {
            scope: scope.to_string(),
            level: line_level(&r.line).to_string(),
            line: r.line.clone(),
            ts_ms: r.ts.timestamp_millis().max(0) as u64,
        };
        let mut v = serde_json::to_value(payload).unwrap_or(Value::Null);
        v["stream"] = json!(match r.stream {
            manager_supervisor::LogStream::Stdout => "stdout",
            manager_supervisor::LogStream::Stderr => "stderr",
        });
        if r.archived {
            v["archived"] = json!(true);
        }
        out.push(v);
    }
    out
}

#[cfg(test)]
mod tests;
