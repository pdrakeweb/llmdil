//! App-side service lifecycle core: one registry of in-flight
//! transitions, operator intent and failures for every managed service
//! (gateway, app-managed PostgreSQL, log receiver, llama-server
//! backends), the shared transition guard every lifecycle operation
//! runs under, the lifecycle markers and events those operations
//! write, one cached SCM snapshot, and the derived per-service views
//! every status surface reads.
//!
//! The vocabulary ([`LifecycleState`], [`TransitionKind`], [`Origin`])
//! and the marker format live in `manager_supervisor::lifecycle`, so
//! the wizard and the service shim write the same lines.
//!
//! Precedence of the derived views (design state model §3), first match
//! wins: an in-flight transition, then the SCM state (with exit codes)
//! for a service-routed instance, then the supervisor state, then the
//! probe / readiness. The merge rules are pure functions
//! ([`merge_backend_liveness`], [`derive_gateway_state`],
//! [`derive_postgres_state`], [`derive_log_receiver_state`]) so they are
//! unit-tested without processes or an SCM.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use manager_services::{ServiceState, ServiceStatus};
use manager_supervisor::lifecycle::{append_marker, marker_line};
pub use manager_supervisor::lifecycle::{LifecycleState, Origin, TransitionKind};
use manager_supervisor::{ProcessState, ProcessStatus};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{events, ApiError, AppState, EventSink};

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Registry key of the LiteLLM gateway.
pub const GATEWAY: &str = "gateway";
/// Registry key of the app-managed PostgreSQL server.
pub const POSTGRES: &str = "postgres";
/// Registry key of the log receiver service.
pub const LOG_RECEIVER: &str = "log-receiver";

/// Registry key of a llama-server backend: `backend:<id>` (the same
/// spelling the service shim's markers use, see
/// `manager_services::lifecycle_key`).
pub fn backend_key(id: &str) -> String {
    format!("backend:{id}")
}

/// The backend id of a `backend:<id>` key.
pub fn backend_id_of(key: &str) -> Option<&str> {
    key.strip_prefix("backend:")
}

/// The service kind a key names, as the SERVICE_LIFECYCLE payload's
/// `kind`: `gateway`, `postgres`, `log-receiver` or `backend`.
fn key_kind(key: &str) -> &str {
    if backend_id_of(key).is_some() {
        "backend"
    } else {
        key
    }
}

/// Operator-facing name of a key ("backend planner", "PostgreSQL").
fn key_label(key: &str) -> String {
    match key {
        GATEWAY => "gateway".to_string(),
        POSTGRES => "PostgreSQL".to_string(),
        LOG_RECEIVER => "log receiver".to_string(),
        other => match backend_id_of(other) {
            Some(id) => format!("backend {id}"),
            None => other.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// What the operator last asked a service to be. An operator stop sets
/// `Stopped` before the kill, so the monitor never reads that exit as a
/// crash; start and restart set `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    Running,
    Stopped,
}

/// One in-flight lifecycle operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub origin: Origin,
    pub since: DateTime<Utc>,
    /// Registry-wide sequence number of the operation; a guard clears
    /// only its own transition.
    #[serde(skip)]
    pub seq: u64,
}

/// The last unexpected failure of a service (a crash, a start that
/// fell back to stopped, a failed start-gateway run). Cleared at the
/// next start or restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub at: DateTime<Utc>,
    pub reason: String,
    pub exit_code: Option<i64>,
}

impl Failure {
    /// A failure observed now.
    pub fn now(reason: impl Into<String>, exit_code: Option<i64>) -> Self {
        Self {
            at: Utc::now(),
            reason: reason.into(),
            exit_code,
        }
    }
}

/// Everything the registry knows about one service.
#[derive(Debug, Clone, Default)]
pub struct ServiceRecord {
    pub transition: Option<Transition>,
    pub intent: Option<Intent>,
    pub failure: Option<Failure>,
    /// Backends: the process pid whose `/health` answered 200 (see
    /// [`BackendLive::process_pid`]). A running backend whose pid is not
    /// the ready pid is still loading its model and reads `starting`.
    pub ready_pid: Option<u32>,
    /// The state the monitor last observed (its diff baseline).
    pub last_observed: Option<LifecycleState>,
    /// Bumped by every begin and every intent change; a probe result
    /// taken under an older generation is discarded.
    pub generation: u64,
    /// When the last transition ended.
    pub last_settled_at: Option<DateTime<Utc>>,
    /// Gateway: the last liveness probe (when, answered).
    pub last_probe: Option<(Instant, bool)>,
}

/// Why [`LifecycleRegistry::try_begin`] refused: another operation
/// holds the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyInfo {
    pub key: String,
    pub kind: TransitionKind,
    pub origin: Origin,
    pub since: DateTime<Utc>,
}

impl std::fmt::Display for BusyInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = (Utc::now() - self.since).num_seconds().max(0);
        write!(
            f,
            "{} {} already in progress (started by {} {secs} s ago)",
            key_label(&self.key),
            self.kind.verb(),
            self.origin
        )
    }
}

/// Per-service lifecycle state shared by every operation and status
/// read. Records sit behind a std mutex held only for short,
/// synchronous reads and writes, so status reads never wait on a
/// running operation; operations serialize per service through
/// per-key async locks taken with `try_lock` (a second operation is
/// refused as busy, never queued).
#[derive(Debug, Default)]
pub struct LifecycleRegistry {
    records: Mutex<HashMap<String, ServiceRecord>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    seq: AtomicU64,
    app_exiting: std::sync::atomic::AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl LifecycleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a transition on `key`: refused with [`BusyInfo`] while
    /// another operation holds the service. The returned guard clears
    /// the transition (and releases the service) when dropped.
    pub fn try_begin(
        self: &Arc<Self>,
        key: &str,
        kind: TransitionKind,
        origin: Origin,
    ) -> Result<TransitionGuard, BusyInfo> {
        let key_lock = lock(&self.locks)
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        // The records lock is held across the try_lock so "lock held"
        // and "transition recorded" change together (the guard's Drop
        // releases both under the same lock).
        let mut records = lock(&self.records);
        let rec = records.entry(key.to_string()).or_default();
        let Ok(owned) = key_lock.try_lock_owned() else {
            let (kind, origin, since) = match &rec.transition {
                Some(t) => (t.kind, t.origin, t.since),
                // Held without a recorded transition (cannot happen
                // through this API; reported conservatively).
                None => (kind, origin, Utc::now()),
            };
            return Err(BusyInfo {
                key: key.to_string(),
                kind,
                origin,
                since,
            });
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let since = Utc::now();
        rec.transition = Some(Transition {
            kind,
            origin,
            since,
            seq,
        });
        rec.generation += 1;
        Ok(TransitionGuard {
            registry: Arc::clone(self),
            key: key.to_string(),
            kind,
            origin,
            seq,
            since,
            lock: Some(owned),
        })
    }

    /// End the transition `seq` on `key` (a newer transition is left
    /// alone) and release `owned`, both under the records lock.
    fn end(&self, key: &str, seq: u64, owned: Option<tokio::sync::OwnedMutexGuard<()>>) {
        let mut records = lock(&self.records);
        if let Some(rec) = records.get_mut(key) {
            if rec.transition.as_ref().is_some_and(|t| t.seq == seq) {
                rec.transition = None;
                rec.last_settled_at = Some(Utc::now());
            }
        }
        drop(owned);
    }

    fn with<R>(&self, key: &str, f: impl FnOnce(&mut ServiceRecord) -> R) -> R {
        let mut records = lock(&self.records);
        f(records.entry(key.to_string()).or_default())
    }

    /// A copy of `key`'s record (a default record when none exists).
    pub fn record(&self, key: &str) -> ServiceRecord {
        lock(&self.records).get(key).cloned().unwrap_or_default()
    }

    /// A copy of every record.
    pub fn records(&self) -> HashMap<String, ServiceRecord> {
        lock(&self.records).clone()
    }

    /// The in-flight transition on `key`, if any.
    pub fn transition(&self, key: &str) -> Option<Transition> {
        lock(&self.records)
            .get(key)
            .and_then(|r| r.transition.clone())
    }

    /// True while any service has a transition in flight.
    pub fn any_transition(&self) -> bool {
        lock(&self.records).values().any(|r| r.transition.is_some())
    }

    /// Record operator intent; bumps the generation.
    pub fn set_intent(&self, key: &str, intent: Intent) {
        self.with(key, |r| {
            r.intent = Some(intent);
            r.generation += 1;
        });
    }

    pub fn intent(&self, key: &str) -> Option<Intent> {
        lock(&self.records).get(key).and_then(|r| r.intent)
    }

    pub fn generation(&self, key: &str) -> u64 {
        lock(&self.records)
            .get(key)
            .map(|r| r.generation)
            .unwrap_or(0)
    }

    pub fn set_failure(&self, key: &str, failure: Failure) {
        self.with(key, |r| r.failure = Some(failure));
    }

    pub fn clear_failure(&self, key: &str) {
        self.with(key, |r| r.failure = None);
    }

    /// Record that `pid` answered its readiness check. Returns whether
    /// this changed the record (the first reporter writes the "ready"
    /// marker; a second one stays quiet).
    pub fn set_ready(&self, key: &str, pid: u32) -> bool {
        self.with(key, |r| r.ready_pid.replace(pid) != Some(pid))
    }

    /// Claim the app-exit bookkeeping: true for the first caller only, so
    /// the update path and the shell's exit event never both write the
    /// "app exiting" markers.
    pub fn claim_app_exit(&self) -> bool {
        !self.app_exiting.swap(true, Ordering::SeqCst)
    }

    /// True once the app-exit bookkeeping was claimed: backends are being
    /// released on purpose, so the monitor must not read them as crashed.
    pub fn app_exiting(&self) -> bool {
        self.app_exiting.load(Ordering::SeqCst)
    }

    /// Record the monitor's observation; returns the previous state
    /// when it changed (`None` on the first observation or no change).
    pub fn observe(&self, key: &str, state: LifecycleState) -> Option<LifecycleState> {
        self.with(key, |r| {
            let prev = r.last_observed.replace(state);
            prev.filter(|p| *p != state)
        })
    }

    /// The gateway probe result recorded within `max_age`, if any.
    pub fn cached_probe(&self, key: &str, max_age: Duration) -> Option<bool> {
        lock(&self.records)
            .get(key)
            .and_then(|r| r.last_probe)
            .filter(|(at, _)| at.elapsed() <= max_age)
            .map(|(_, up)| up)
    }

    /// Record a probe result (the monitor and the status reads share it).
    pub fn set_probe(&self, key: &str, up: bool) {
        self.with(key, |r| r.last_probe = Some((Instant::now(), up)));
    }
}

/// RAII handle of one in-flight transition. Dropping it (on any exit
/// path) clears the transition — unless a newer one replaced it — and
/// releases the service for the next operation.
#[derive(Debug)]
pub struct TransitionGuard {
    registry: Arc<LifecycleRegistry>,
    key: String,
    kind: TransitionKind,
    origin: Origin,
    seq: u64,
    since: DateTime<Utc>,
    lock: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl TransitionGuard {
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn kind(&self) -> TransitionKind {
        self.kind
    }
    pub fn origin(&self) -> Origin {
        self.origin
    }
    pub fn seq(&self) -> u64 {
        self.seq
    }
    pub fn since(&self) -> DateTime<Utc> {
        self.since
    }
}

impl Drop for TransitionGuard {
    fn drop(&mut self) {
        let owned = self.lock.take();
        self.registry.end(&self.key, self.seq, owned);
    }
}

// ---------------------------------------------------------------------------
// Markers and events
// ---------------------------------------------------------------------------

/// The lifecycle marker log of `key`, merged into that service's own
/// log scope by `get_logs`: `logs/gateway.lifecycle.log`,
/// `logs/postgres.lifecycle.log`, `logs/backend-<id>.lifecycle.log`, and
/// the receiver's `receiver.lifecycle.log` (the same files the service
/// shim writes in service mode).
pub fn log_path(data_dir: &Path, key: &str) -> PathBuf {
    match key {
        GATEWAY => manager_wizard::gateway_lifecycle_log(data_dir),
        POSTGRES => manager_wizard::postgres_lifecycle_log(data_dir),
        LOG_RECEIVER => {
            let def =
                manager_services::log_receiver_definition(data_dir, Path::new("receiver.exe"));
            manager_services::lifecycle_log_path(&def).unwrap_or_else(|| {
                manager_services::log_receiver_dir(data_dir)
                    .join("logs")
                    .join("receiver.lifecycle.log")
            })
        }
        other => {
            let stem = match backend_id_of(other) {
                Some(id) => format!("backend-{id}"),
                None => other.replace([':', '/', '\\'], "-"),
            };
            data_dir.join("logs").join(format!("{stem}.lifecycle.log"))
        }
    }
}

/// Append one lifecycle marker for `key` to its own log. Best-effort.
pub fn mark(
    data_dir: &Path,
    key: &str,
    event: &str,
    detail: Option<&str>,
    origin: Option<Origin>,
) {
    let line = marker_line(Utc::now(), key, event, detail, origin);
    append_marker(&log_path(data_dir, key), &line);
}

/// The SERVICE_LIFECYCLE payload (design state model §6).
pub fn event_payload(
    key: &str,
    event: &str,
    state: LifecycleState,
    origin: Option<Origin>,
    detail: Option<&str>,
) -> Value {
    json!({
        "key": key,
        "kind": key_kind(key),
        "id": backend_id_of(key),
        "event": event,
        "state": state,
        "origin": origin,
        "detail": detail,
        "at": Utc::now().to_rfc3339(),
    })
}

/// Emit one lifecycle event: SERVICE_LIFECYCLE for every key, plus the
/// legacy BACKEND_STATUS `{id, event, state, origin, error?}` for
/// backend keys (callers emit through here only, never both).
pub fn emit(
    sink: &dyn EventSink,
    key: &str,
    event: &str,
    state: LifecycleState,
    origin: Option<Origin>,
    detail: Option<&str>,
) {
    sink.emit(
        events::SERVICE_LIFECYCLE,
        event_payload(key, event, state, origin, detail),
    );
    if let Some(id) = backend_id_of(key) {
        let mut payload = json!({
            "id": id,
            "event": event,
            "state": state,
            "origin": origin,
        });
        if matches!(event, "failed" | "crashed") {
            payload["error"] = json!(detail);
        }
        sink.emit(events::BACKEND_STATUS, payload);
    }
}

/// Begin a lifecycle operation on `key`: take the service's transition
/// guard (a running operation is refused with [`ApiError::Busy`]),
/// write the "`<verb>` requested" marker, emit the transition event and
/// invalidate the SCM snapshot. The previous failure stays recorded
/// until [`finish`] reports success.
pub async fn begin(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    key: &str,
    kind: TransitionKind,
    origin: Origin,
    detail: Option<&str>,
) -> Result<TransitionGuard, ApiError> {
    let guard = state
        .lifecycle
        .try_begin(key, kind, origin)
        .map_err(|busy| ApiError::Busy(busy.to_string()))?;
    // The previous failure is kept until the operation succeeds
    // ([`finish`] clears it): a start refused or abandoned after this
    // point must not make a failed service read `stopped`.
    // The event below reports this state; the monitor's diff baseline
    // follows so it does not report the same change a second time.
    state.lifecycle.observe(key, kind.into());
    mark(
        &state.data_dir,
        key,
        &format!("{} requested", kind.verb()),
        detail,
        Some(origin),
    );
    emit(
        sink.as_ref(),
        key,
        kind.as_str(),
        kind.into(),
        Some(origin),
        detail,
    );
    invalidate_scm(state);
    Ok(guard)
}

/// The terminal event of a transition kind on success.
fn done_event(kind: TransitionKind) -> &'static str {
    match kind {
        TransitionKind::Starting => "started",
        TransitionKind::Stopping => "stopped",
        TransitionKind::Restarting => "restarted",
    }
}

/// End a lifecycle operation: write the outcome marker ("started: …" /
/// "stopped: …" / "restarted: …", or "`<verb>` failed: reason"), emit
/// the terminal event, record or clear the failure, and drop the guard.
pub fn finish(
    state: &AppState,
    sink: &dyn EventSink,
    guard: TransitionGuard,
    result: Result<String, String>,
) {
    finish_with(state, sink, guard, result, None);
}

/// The state a successful operation of `kind` settles in: `stopped` for
/// a stop, else `ready_state` (default `running`; a backend start that
/// did not wait for readiness passes `starting`, so event-driven
/// surfaces never show `running` and then flip back to `starting`).
pub fn settled_state(kind: TransitionKind, ready_state: Option<LifecycleState>) -> LifecycleState {
    if kind == TransitionKind::Stopping {
        LifecycleState::Stopped
    } else {
        ready_state.unwrap_or(LifecycleState::Running)
    }
}

/// [`finish`] with the state a successful start/restart settles in
/// (see [`settled_state`]).
pub fn finish_with(
    state: &AppState,
    sink: &dyn EventSink,
    guard: TransitionGuard,
    result: Result<String, String>,
    ready_state: Option<LifecycleState>,
) {
    let key = guard.key().to_string();
    let kind = guard.kind();
    let origin = guard.origin();
    // Release first so a status read right after the event already
    // sees the settled state.
    drop(guard);
    match result {
        Ok(detail) => {
            let detail = Some(detail.as_str()).filter(|d| !d.is_empty());
            let event = done_event(kind);
            if kind != TransitionKind::Stopping {
                state.lifecycle.clear_failure(&key);
            }
            let settled = settled_state(kind, ready_state);
            state.lifecycle.observe(&key, settled);
            mark(&state.data_dir, &key, event, detail, Some(origin));
            emit(sink, &key, event, settled, Some(origin), detail);
        }
        Err(reason) => {
            state
                .lifecycle
                .set_failure(&key, Failure::now(reason.clone(), None));
            state.lifecycle.observe(&key, LifecycleState::Failed);
            mark(
                &state.data_dir,
                &key,
                &format!("{} failed", kind.verb()),
                Some(&reason),
                Some(origin),
            );
            emit(
                sink,
                &key,
                "failed",
                LifecycleState::Failed,
                Some(origin),
                Some(&reason),
            );
        }
    }
    invalidate_scm(state);
}

/// The single "did this lifecycle operation succeed" rule for op
/// payloads: a payload with `busy: true` or `ok: false` failed, and the
/// reason is its `message` / `error` / `reason` text. `None` means
/// success.
pub fn payload_failure(value: &Value) -> Option<String> {
    let busy = value.get("busy").and_then(Value::as_bool) == Some(true);
    let not_ok = value.get("ok").and_then(Value::as_bool) == Some(false);
    if !busy && !not_ok {
        return None;
    }
    let text = ["message", "error", "reason"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    Some(match (text, busy) {
        (Some(t), _) => t,
        (None, true) => match value.get("running_step").and_then(Value::as_str) {
            Some(step) => format!("busy: {step} is running"),
            None => "busy: another operation is running".to_string(),
        },
        (None, false) => "the operation failed".to_string(),
    })
}

// ---------------------------------------------------------------------------
// SCM snapshot
// ---------------------------------------------------------------------------

/// The cached SCM status of every service the app manages.
pub type ScmSnapshot = Arc<HashMap<String, ServiceStatus>>;

/// One batched SCM query covering the gateway, the log receiver and
/// every configured backend's service, cached on [`AppState`] for
/// `max_age` (lifecycle operations invalidate it). Services the SCM
/// does not know are present as `NotInstalled`; platforms without an
/// SCM yield an empty map. Live rows carry `child_pid` from the
/// service's pid file (`gateway.pid`, `logs/backend-<id>.pid`).
pub async fn scm_snapshot(state: &AppState, max_age: Duration) -> ScmSnapshot {
    if let Some(snap) = cached_scm(state, max_age) {
        return snap;
    }
    let backend_ids: Vec<String> = state
        .config
        .read()
        .await
        .backends
        .iter()
        .map(|b| b.id.clone())
        .collect();
    scm_snapshot_ids(state, max_age, backend_ids).await
}

fn cached_scm(state: &AppState, max_age: Duration) -> Option<ScmSnapshot> {
    lock(&state.scm_cache)
        .as_ref()
        .filter(|(at, _)| at.elapsed() <= max_age)
        .map(|(_, snap)| Arc::clone(snap))
}

/// [`scm_snapshot`] for an explicit backend id list: never touches the
/// config lock, so callers holding a config guard can use it.
pub async fn scm_snapshot_ids(
    state: &AppState,
    max_age: Duration,
    backend_ids: Vec<String>,
) -> ScmSnapshot {
    if let Some(snap) = cached_scm(state, max_age) {
        return snap;
    }
    let data_dir = state.data_dir.clone();
    let snap = tokio::task::spawn_blocking(move || query_scm(&data_dir, &backend_ids))
        .await
        .unwrap_or_default();
    let snap = Arc::new(snap);
    *lock(&state.scm_cache) = Some((Instant::now(), Arc::clone(&snap)));
    snap
}

pub(crate) fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
}

fn query_scm(data_dir: &Path, backend_ids: &[String]) -> HashMap<String, ServiceStatus> {
    use manager_services::ServiceControl as _;
    let control = manager_services::scm::platform_control();
    let mut names: Vec<(String, Option<PathBuf>)> = vec![
        (
            manager_services::GATEWAY_SERVICE_NAME.to_string(),
            Some(manager_wizard::gateway_pid_path(data_dir)),
        ),
        (manager_services::LOG_RECEIVER_SERVICE_NAME.to_string(), None),
    ];
    for id in backend_ids {
        names.push((
            manager_services::backend_service_name(id),
            Some(data_dir.join("logs").join(format!("backend-{id}.pid"))),
        ));
    }
    let mut out = HashMap::new();
    for (name, pid_file) in names {
        let Ok(mut status) = control.query(&name) else {
            continue;
        };
        if status.child_pid.is_none()
            && matches!(
                status.state,
                ServiceState::Running | ServiceState::StartPending | ServiceState::StopPending
            )
        {
            status.child_pid = pid_file
                .as_deref()
                .and_then(read_pid_file)
                .filter(|p| manager_supervisor::pid_alive(*p));
        }
        out.insert(name, status);
    }
    out
}

/// Drop the cached SCM snapshot so the next read queries afresh.
pub fn invalidate_scm(state: &AppState) {
    *lock(&state.scm_cache) = None;
}

/// The SCM status of one service: from the snapshot (at most `max_age`
/// old) when it covers `name`, else one direct query on the blocking
/// pool (a service outside the snapshot, e.g. an unconfigured
/// backend's leftover registration). `None` when the query fails.
pub async fn scm_status(state: &AppState, name: &str, max_age: Duration) -> Option<ServiceStatus> {
    let snap = scm_snapshot(state, max_age).await;
    if let Some(s) = snap.get(name) {
        return Some(s.clone());
    }
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        use manager_services::ServiceControl as _;
        manager_services::scm::platform_control().query(&name).ok()
    })
    .await
    .ok()
    .flatten()
}

/// True when `status` is a real registration (not `NotInstalled`).
pub(crate) fn registered(status: &ServiceStatus) -> bool {
    !matches!(status.state, ServiceState::NotInstalled)
}

/// Whether an SCM status counts as a live instance.
pub(crate) fn scm_live(status: &ServiceStatus) -> bool {
    matches!(
        status.state,
        ServiceState::Running
            | ServiceState::StartPending
            | ServiceState::StopPending
            | ServiceState::Paused
    )
}

/// Exit code of a stopped SCM status: the service-specific code when the
/// shim reported one, else the Win32 code; `None` for a clean stop.
pub(crate) fn scm_exit_code(status: &ServiceStatus) -> Option<i64> {
    if !status.stopped_with_error() {
        return None;
    }
    if status.win32_exit_code == manager_services::ERROR_SERVICE_SPECIFIC_ERROR {
        Some(status.service_specific_exit_code as i64)
    } else {
        Some(status.win32_exit_code as i64)
    }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// The one liveness view of a backend, in both run modes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackendLive {
    pub id: String,
    pub state: LifecycleState,
    /// Supervisor-owned: the llama-server pid. Service-managed: the
    /// service host (shim) pid.
    pub pid: Option<u32>,
    /// Service-managed: the llama-server child pid (from its pid file).
    pub child_pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub uptime_secs: Option<u64>,
    pub service_managed: bool,
    pub adopted: bool,
    pub restarts: u32,
    pub failure_reason: Option<String>,
    pub transition: Option<Transition>,
}

impl BackendLive {
    /// The pid of the llama-server process itself (the child under a
    /// service host, else `pid`): the pid readiness is recorded for
    /// ([`LifecycleRegistry::set_ready`]).
    pub fn process_pid(&self) -> Option<u32> {
        self.child_pid.or(self.pid)
    }
}

/// Merge the supervisor, the SCM snapshot and the registry into one
/// row per backend (design state model §3), first match wins:
/// 1. an in-flight transition → its kind;
/// 2. a registered SCM service → `lifecycle_state()` with the exit-code
///    reason (in process mode only while the service is live and the
///    supervisor has no live process of its own);
/// 3. the supervisor state (a recorded failure turns a stopped row with
///    intent other than Stopped into `failed`);
/// 4. running but its process is not the ready pid → `starting` (model
///    still loading); adopted processes count as ready.
///
/// Rows cover `configured` ids plus `sup` rows that are live (orphans);
/// `start_of` resolves a service process's start instant.
pub fn merge_backend_liveness(
    sup: &[ProcessStatus],
    scm: &HashMap<String, ServiceStatus>,
    records: &HashMap<String, ServiceRecord>,
    configured: &[(String, u16)],
    service_mode: bool,
    now: DateTime<Utc>,
    start_of: &dyn Fn(u32) -> Option<DateTime<Utc>>,
) -> HashMap<String, BackendLive> {
    let mut ids: Vec<String> = configured.iter().map(|(id, _)| id.clone()).collect();
    for s in sup {
        // Live orphans, plus an orphan with an operation in flight (a
        // removed-but-running backend being stopped is detached from
        // the supervisor during the drain and must still read
        // "stopping", not vanish).
        let in_flight = records
            .get(&backend_key(&s.id))
            .is_some_and(|r| r.transition.is_some());
        if !ids.contains(&s.id) && (s.pid.is_some() || in_flight) {
            ids.push(s.id.clone());
        }
    }
    let empty = ServiceRecord::default();
    let mut out = HashMap::new();
    for id in ids {
        let rec = records.get(&backend_key(&id)).unwrap_or(&empty);
        let sup_row = sup.iter().find(|s| s.id == id);
        let sup_live = sup_row.is_some_and(|s| {
            s.pid.is_some()
                && matches!(
                    s.state,
                    ProcessState::Running | ProcessState::Starting | ProcessState::Stopping
                )
        });
        let scm_row = scm
            .get(&manager_services::backend_service_name(&id))
            .filter(|s| registered(s))
            .filter(|s| service_mode || (scm_live(s) && !sup_live));
        let mut row = match scm_row {
            Some(s) => {
                let started_at = if scm_live(s) {
                    s.child_pid.or(s.pid).and_then(start_of)
                } else {
                    None
                };
                BackendLive {
                    id: id.clone(),
                    state: s.lifecycle_state(),
                    pid: s.pid,
                    child_pid: s.child_pid,
                    started_at,
                    uptime_secs: None,
                    service_managed: true,
                    adopted: false,
                    restarts: 0,
                    failure_reason: s.failure_reason(),
                    transition: None,
                }
            }
            None => match sup_row {
                Some(s) => BackendLive {
                    id: id.clone(),
                    state: s.state.into(),
                    pid: s.pid,
                    child_pid: None,
                    started_at: s
                        .uptime
                        .map(|u| now - chrono::Duration::from_std(u).unwrap_or_default()),
                    uptime_secs: None,
                    service_managed: false,
                    adopted: s.adopted,
                    restarts: s.restarts,
                    failure_reason: s.failure_reason.clone(),
                    transition: None,
                },
                None => BackendLive {
                    id: id.clone(),
                    state: LifecycleState::Stopped,
                    pid: None,
                    child_pid: None,
                    started_at: None,
                    uptime_secs: None,
                    service_managed: false,
                    adopted: false,
                    restarts: 0,
                    failure_reason: None,
                    transition: None,
                },
            },
        };
        // A crash the monitor recorded keeps the row failed until the
        // next start, unless the operator stopped it.
        if let Some(f) = &rec.failure {
            if matches!(row.state, LifecycleState::Stopped | LifecycleState::Failed)
                && rec.intent != Some(Intent::Stopped)
            {
                row.state = LifecycleState::Failed;
                if row.failure_reason.is_none() {
                    row.failure_reason = Some(f.reason.clone());
                }
            }
        }
        if row.state == LifecycleState::Running
            && !row.adopted
            && row.process_pid().is_some()
            && rec.ready_pid != row.process_pid()
        {
            row.state = LifecycleState::Starting;
        }
        if let Some(t) = &rec.transition {
            row.state = t.kind.into();
            row.transition = Some(t.clone());
        }
        row.uptime_secs = row
            .started_at
            .map(|s| (now - s).num_seconds().max(0) as u64);
        out.insert(id, row);
    }
    out
}

/// Every backend's liveness, both modes (supervisor + cached SCM
/// snapshot + registry). Takes the supervisor mutex once, briefly, for
/// `status()` only: reaping exits is the monitor's job.
pub async fn backend_liveness(state: &AppState) -> HashMap<String, BackendLive> {
    let cfg = state.config.read().await.clone();
    backend_liveness_for(state, &cfg).await
}

/// [`backend_liveness`] against an explicit config: never touches the
/// config lock, so callers that hold a config guard (the activation
/// journal writers, gateway status) can read liveness without
/// deadlocking on it.
pub async fn backend_liveness_for(
    state: &AppState,
    cfg: &manager_config::AppConfig,
) -> HashMap<String, BackendLive> {
    let (configured, service_mode) = (
        cfg.backends
            .iter()
            .map(|b| (b.id.clone(), b.port))
            .collect::<Vec<_>>(),
        cfg.supervision.mode == manager_config::SupervisionMode::Service,
    );
    let owned: std::collections::HashSet<String> = lock(&state.backend_spec_ids).clone();
    let sup: Vec<ProcessStatus> = {
        let sup = state.supervisor().await.lock().await;
        sup.status()
    }
    .into_iter()
    .filter(|s| owned.contains(&s.id) || configured.iter().any(|(id, _)| *id == s.id))
    .collect();
    let ids: Vec<String> = configured.iter().map(|(id, _)| id.clone()).collect();
    let scm = scm_snapshot_ids(state, Duration::from_secs(1), ids).await;
    let records = state.lifecycle.records();
    let start_of = |pid: u32| {
        manager_supervisor::lifecycle::process_start_time(pid).map(DateTime::<Utc>::from)
    };
    merge_backend_liveness(
        &sup,
        &scm,
        &records,
        &configured,
        service_mode,
        Utc::now(),
        &start_of,
    )
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

/// How long after a start a gateway whose process is alive but not
/// answering still reads `starting` (LiteLLM imports, Prisma, model
/// list) before it reads `degraded`.
pub const GATEWAY_START_GRACE: Duration = Duration::from_secs(120);

/// The inputs of [`derive_gateway_state`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GatewayInputs {
    pub transition: Option<TransitionKind>,
    /// The gateway is SCM-routed (service mode and registered).
    pub service_route: bool,
    /// The SCM's lifecycle state of the gateway service, when registered.
    pub scm_state: Option<LifecycleState>,
    pub probe_up: bool,
    /// A live gateway process exists (pid file, else the port owner).
    pub pid_alive: bool,
    /// The last start settled (or the process started) within
    /// [`GATEWAY_START_GRACE`].
    pub recent_start: bool,
    pub intent: Option<Intent>,
    pub has_failure: bool,
}

/// The gateway's lifecycle state (design state model §3), first match
/// wins: transition; SCM start/stop pending on the service route; probe
/// up → running; live process (or, on the service route, an SCM-running
/// service host) → starting within the start grace, else degraded; an SCM-reported failure or a recorded failure with intent
/// Running → failed; otherwise stopped.
pub fn derive_gateway_state(i: GatewayInputs) -> LifecycleState {
    if let Some(kind) = i.transition {
        return kind.into();
    }
    if i.service_route {
        if let Some(s @ (LifecycleState::Starting | LifecycleState::Stopping)) = i.scm_state {
            return s;
        }
    }
    if i.probe_up {
        return LifecycleState::Running;
    }
    // On the service route an SCM-running service host counts as a live
    // process even with no gateway pid (its child not spawned yet or
    // being respawned, the pid file not written, the port not bound):
    // reading that as `stopped` would classify a running service as an
    // operator stop.
    let host_live = i.service_route
        && matches!(
            i.scm_state,
            Some(LifecycleState::Running | LifecycleState::Degraded)
        );
    if i.pid_alive || host_live {
        return if i.recent_start {
            LifecycleState::Starting
        } else {
            LifecycleState::Degraded
        };
    }
    if i.service_route
        && i.scm_state == Some(LifecycleState::Failed)
        && i.intent != Some(Intent::Stopped)
    {
        return LifecycleState::Failed;
    }
    if i.intent == Some(Intent::Running) && i.has_failure {
        return LifecycleState::Failed;
    }
    LifecycleState::Stopped
}

/// The gateway's derived view.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayView {
    pub state: LifecycleState,
    /// The liveness probe answered.
    pub up: bool,
    pub pid: Option<u32>,
    /// How `pid` was found: `pid_file` or `port_owner`.
    pub found_by: Option<&'static str>,
    pub service_managed: bool,
    pub transition: Option<Transition>,
    pub intent: Option<Intent>,
    pub failure: Option<Failure>,
    pub probed_at: DateTime<Utc>,
    /// The registry generation the view was derived under (the monitor
    /// discards a result whose generation moved on).
    #[serde(skip)]
    pub generation: u64,
}

/// How long a gateway probe result is reused.
pub const GATEWAY_PROBE_MAX_AGE: Duration = Duration::from_secs(2);

/// The gateway's derived view: transition, SCM state on the service
/// route, the liveness probe (reused when younger than 2 s), the live
/// gateway pid (`manager_wizard::resolve_gateway_pid`), intent and the
/// recorded failure.
pub async fn gateway_view(state: &AppState) -> GatewayView {
    let (port, probe_host, service_mode) = {
        let cfg = state.config.read().await;
        (
            cfg.gateway.port,
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.supervision.mode == manager_config::SupervisionMode::Service,
        )
    };
    let rec = state.lifecycle.record(GATEWAY);
    let scm = scm_snapshot(state, Duration::from_secs(1)).await;
    let scm_row = scm
        .get(manager_services::GATEWAY_SERVICE_NAME)
        .filter(|s| registered(s));
    let service_route = service_mode && scm_row.is_some();
    let up = match state.lifecycle.cached_probe(GATEWAY, GATEWAY_PROBE_MAX_AGE) {
        Some(up) => up,
        None => {
            let up = tokio::task::spawn_blocking(move || crate::probe_gateway(&probe_host, port))
                .await
                .unwrap_or(false);
            state.lifecycle.set_probe(GATEWAY, up);
            up
        }
    };
    let data_dir = state.data_dir.clone();
    let resolved =
        tokio::task::spawn_blocking(move || manager_wizard::resolve_gateway_pid(&data_dir, port))
            .await
            .unwrap_or(None);
    let now = Utc::now();
    let grace = chrono::Duration::from_std(GATEWAY_START_GRACE).unwrap_or_default();
    let process_recent = resolved
        .and_then(|(pid, _)| manager_supervisor::lifecycle::process_start_time(pid))
        .map(|t| now - DateTime::<Utc>::from(t) <= grace)
        .unwrap_or(false);
    let settled_recent = rec.last_settled_at.is_some_and(|t| now - t <= grace);
    let inputs = GatewayInputs {
        transition: rec.transition.as_ref().map(|t| t.kind),
        service_route,
        scm_state: scm_row.map(ServiceStatus::lifecycle_state),
        probe_up: up,
        pid_alive: resolved.is_some(),
        recent_start: process_recent || settled_recent,
        intent: rec.intent,
        has_failure: rec.failure.is_some(),
    };
    let mut failure = rec.failure.clone();
    if failure.is_none() && service_route {
        if let Some(s) = scm_row.filter(|s| s.stopped_with_error()) {
            failure = s.failure_reason().map(|reason| Failure {
                at: now,
                reason,
                exit_code: scm_exit_code(s),
            });
        }
    }
    GatewayView {
        state: derive_gateway_state(inputs),
        up,
        pid: resolved.map(|(pid, _)| pid),
        found_by: resolved.map(|(_, by)| match by {
            manager_wizard::FoundBy::PidFile => "pid_file",
            manager_wizard::FoundBy::PortOwner => "port_owner",
            manager_wizard::FoundBy::None => "none",
        }),
        service_managed: service_route,
        transition: rec.transition,
        intent: rec.intent,
        failure,
        probed_at: now,
        generation: rec.generation,
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

/// The app-managed PostgreSQL server's derived view.
#[derive(Debug, Clone, Serialize)]
pub struct PostgresView {
    pub installed: bool,
    pub state: LifecycleState,
    pub pid: Option<u32>,
    /// The configured port.
    pub port: u16,
    /// The port the live postmaster listens on, when it differs from
    /// the configured one (the next gateway start restarts it).
    pub port_mismatch: Option<u16>,
    pub transition: Option<Transition>,
}

/// PostgreSQL's lifecycle state: not installed; a transition; our
/// postmaster alive and accepting → running; alive and listening on a
/// port other than the configured one (`port_mismatch`: the probe of
/// the configured port can never answer) → running, awaiting the
/// port-change restart; alive but not accepting → starting; something
/// accepting on the port without our pid → running; expected up
/// (intent Running) but gone → failed; else stopped.
pub fn derive_postgres_state(
    installed: bool,
    transition: Option<TransitionKind>,
    pid_alive: bool,
    accepting: bool,
    expected_running: bool,
    port_mismatch: bool,
) -> LifecycleState {
    if !installed {
        return LifecycleState::NotInstalled;
    }
    if let Some(kind) = transition {
        return kind.into();
    }
    match (pid_alive, accepting) {
        (true, true) | (false, true) => LifecycleState::Running,
        (true, false) if port_mismatch => LifecycleState::Running,
        (true, false) => LifecycleState::Starting,
        (false, false) if expected_running => LifecycleState::Failed,
        (false, false) => LifecycleState::Stopped,
    }
}

/// PostgreSQL's view (postmaster pid + a 300 ms TCP connect; never
/// `pg_isready`). It is expected up while its own intent is Running,
/// or — with no intent of its own — while the gateway's is.
pub async fn postgres_view(state: &AppState) -> PostgresView {
    let cfg = state.config.read().await.clone();
    let ops = manager_wizard::ops::RealSystemOps::default();
    let st = manager_wizard::steps::postgres_status(&ops, &cfg, &state.data_dir).await;
    let rec = state.lifecycle.record(POSTGRES);
    let expected = match rec.intent {
        Some(i) => i == Intent::Running,
        None => state.lifecycle.intent(GATEWAY) == Some(Intent::Running),
    };
    PostgresView {
        installed: st.installed,
        state: derive_postgres_state(
            st.installed,
            rec.transition.as_ref().map(|t| t.kind),
            st.pid_alive,
            st.accepting,
            expected,
            st.port_mismatch(),
        ),
        pid: st.pid.filter(|_| st.pid_alive),
        port: st.configured_port,
        port_mismatch: if st.port_mismatch() { st.pid_port } else { None },
        transition: rec.transition,
    }
}

// ---------------------------------------------------------------------------
// Log receiver
// ---------------------------------------------------------------------------

/// The receiver's local ingest port (`log_receiver::LOCAL_RECEIVER_ENDPOINT`).
pub const LOG_RECEIVER_INGEST_PORT: u16 = 8473;

/// The log receiver's derived view.
#[derive(Debug, Clone, Serialize)]
pub struct LogReceiverView {
    pub registered: bool,
    pub state: LifecycleState,
    pub pid: Option<u32>,
    pub exit_code: Option<i64>,
    pub failure_reason: Option<String>,
    pub transition: Option<Transition>,
}

/// The receiver's lifecycle state: a transition; else the SCM state
/// (`not_installed` without a registration), with `running` downgraded
/// to `degraded` while the ingest port refuses connections.
pub fn derive_log_receiver_state(
    transition: Option<TransitionKind>,
    scm: Option<&ServiceStatus>,
    ingest_ok: bool,
) -> LifecycleState {
    if let Some(kind) = transition {
        return kind.into();
    }
    match scm.map(ServiceStatus::lifecycle_state) {
        None => LifecycleState::NotInstalled,
        Some(LifecycleState::Running) if !ingest_ok => LifecycleState::Degraded,
        Some(s) => s,
    }
}

/// The log receiver's view (cached SCM snapshot + an ingest connect).
pub async fn log_receiver_view(state: &AppState) -> LogReceiverView {
    let scm = scm_snapshot(state, Duration::from_secs(1)).await;
    let row = scm
        .get(manager_services::LOG_RECEIVER_SERVICE_NAME)
        .filter(|s| registered(s))
        .cloned();
    let ingest_ok = if row
        .as_ref()
        .is_some_and(|s| s.state == ServiceState::Running)
    {
        tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(("127.0.0.1", LOG_RECEIVER_INGEST_PORT)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    } else {
        false
    };
    let rec = state.lifecycle.record(LOG_RECEIVER);
    LogReceiverView {
        registered: row.is_some(),
        state: derive_log_receiver_state(
            rec.transition.as_ref().map(|t| t.kind),
            row.as_ref(),
            ingest_ok,
        ),
        pid: row.as_ref().and_then(|s| s.pid),
        exit_code: row.as_ref().and_then(scm_exit_code),
        failure_reason: row.as_ref().and_then(ServiceStatus::failure_reason),
        transition: rec.transition,
    }
}

// ---------------------------------------------------------------------------
// App start and exit
// ---------------------------------------------------------------------------

/// The intent to seed at app start for a service observed `live` (pure):
/// Running when nothing is recorded yet, so the monitor reports a later
/// crash of a service this session did not start; `None` (leave it) when
/// an intent exists or the service is not live.
pub fn seed_intent(current: Option<Intent>, live: bool) -> Option<Intent> {
    (current.is_none() && live).then_some(Intent::Running)
}

/// The marker event of a service found live at app start.
fn observed_at_start(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("observed running at app start (pid {pid})"),
        None => "observed running at app start".to_string(),
    }
}

/// Reconcile the registry with what is running when the app starts, once,
/// before the lifecycle monitor's first tick: register backend specs and
/// adopt orphaned llama-servers (BE-10), seed intent Running for the
/// gateway and every live backend, set each service's diff baseline, and
/// write "observed running at app start (pid N)" into each live
/// service's own log.
pub async fn startup_reconcile(state: &AppState, sink: &dyn EventSink) {
    let adopted = crate::adopt_orphan_backends(state).await;
    crate::emit_adopted(sink, &adopted);
    let data_dir = state.data_dir.clone();

    let rows = backend_liveness(state).await;
    let mut ids: Vec<&String> = rows.keys().collect();
    ids.sort();
    for id in ids {
        let row = &rows[id];
        let key = backend_key(id);
        state.lifecycle.observe(&key, row.state);
        if adopted.iter().any(|(a, _, _)| a == id) {
            // adopt_orphan_backends wrote its own marker and intent.
            continue;
        }
        if let Some(intent) = seed_intent(state.lifecycle.intent(&key), row.state.is_live()) {
            state.lifecycle.set_intent(&key, intent);
            mark(
                &data_dir,
                &key,
                &observed_at_start(row.process_pid()),
                None,
                Some(Origin::Startup),
            );
        }
    }

    let gw = gateway_view(state).await;
    state.lifecycle.observe(GATEWAY, gw.state);
    if let Some(intent) = seed_intent(
        state.lifecycle.intent(GATEWAY),
        gw.up || gw.pid.is_some(),
    ) {
        state.lifecycle.set_intent(GATEWAY, intent);
        mark(
            &data_dir,
            GATEWAY,
            &observed_at_start(gw.pid),
            None,
            Some(Origin::Startup),
        );
    }

    let pg = postgres_view(state).await;
    if pg.installed {
        state.lifecycle.observe(POSTGRES, pg.state);
        if pg.state.is_live() {
            mark(
                &data_dir,
                POSTGRES,
                &observed_at_start(pg.pid),
                None,
                Some(Origin::Startup),
            );
        }
    }

    let lr = log_receiver_view(state).await;
    state.lifecycle.observe(LOG_RECEIVER, lr.state);
    if lr.registered && lr.state.is_live() {
        mark(
            &data_dir,
            LOG_RECEIVER,
            &observed_at_start(lr.pid),
            None,
            Some(Origin::Startup),
        );
    }
}

/// The marker event of a service left running when the app exits.
pub fn exit_marker(reason: &str, pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("app exiting ({reason}); left running (pid {pid})"),
        None => format!("app exiting ({reason}); left running"),
    }
}

/// How long the exit bookkeeping may hold up the app's exit.
const APP_EXIT_BUDGET: Duration = Duration::from_secs(5);

/// The app is exiting (`reason`: "exit" from the shell, "update" before
/// the self-update restart). Every managed service is deliberately left
/// running (OS-9 / ACT-12): each live one gets an "app exiting (reason);
/// left running (pid N)" marker in its own log, process-mode backends
/// are released from the supervisor without killing them (the next
/// session adopts them), and the log shipper is shut down. Runs once;
/// bounded so a slow probe cannot hold the exit.
pub async fn on_app_exit(state: &AppState, reason: &str) {
    if !state.lifecycle.claim_app_exit() {
        return;
    }
    let work = async {
        let data_dir = state.data_dir.clone();
        let rows = backend_liveness(state).await;
        for row in rows.values().filter(|r| r.state.is_live()) {
            mark(
                &data_dir,
                &backend_key(&row.id),
                &exit_marker(reason, row.process_pid()),
                None,
                None,
            );
        }
        let port = state.config.read().await.gateway.port;
        let dd = data_dir.clone();
        let gateway_pid =
            tokio::task::spawn_blocking(move || manager_wizard::resolve_gateway_pid(&dd, port))
                .await
                .ok()
                .flatten();
        if let Some((pid, _)) = gateway_pid {
            mark(&data_dir, GATEWAY, &exit_marker(reason, Some(pid)), None, None);
        }
        let pg = postgres_view(state).await;
        if pg.installed && pg.state.is_live() {
            mark(&data_dir, POSTGRES, &exit_marker(reason, pg.pid), None, None);
        }
        let scm = scm_snapshot(state, Duration::from_secs(1)).await;
        if let Some(s) = scm
            .get(manager_services::LOG_RECEIVER_SERVICE_NAME)
            .filter(|s| scm_live(s))
        {
            mark(&data_dir, LOG_RECEIVER, &exit_marker(reason, s.pid), None, None);
        }
        // Keep-running is explicit, not an accident of process exit
        // skipping destructors: release the supervised backend processes
        // without running their kill-on-drop.
        let owned: Vec<String> = lock(&state.backend_spec_ids).iter().cloned().collect();
        let mut sup = state.supervisor().await.lock().await;
        for id in owned {
            if let Ok(proc) = sup.detach(&id) {
                std::mem::forget(proc);
            }
        }
    };
    if tokio::time::timeout(APP_EXIT_BUDGET, work).await.is_err() {
        tracing::warn!("app exit bookkeeping timed out");
    }
    crate::logship::stop_log_streaming(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_seeds_intent_only_for_live_services_without_one() {
        // An up gateway at app start becomes expected up.
        assert_eq!(seed_intent(None, true), Some(Intent::Running));
        assert_eq!(seed_intent(None, false), None);
        // An intent recorded by an operation is never overwritten.
        assert_eq!(seed_intent(Some(Intent::Stopped), true), None);
        assert_eq!(seed_intent(Some(Intent::Running), true), None);
        assert_eq!(
            observed_at_start(Some(42)),
            "observed running at app start (pid 42)"
        );
        assert_eq!(
            exit_marker("update", Some(7)),
            "app exiting (update); left running (pid 7)"
        );
        assert_eq!(exit_marker("exit", None), "app exiting (exit); left running");
    }

    #[test]
    fn app_exit_is_claimed_once() {
        let reg = LifecycleRegistry::new();
        assert!(reg.claim_app_exit());
        assert!(!reg.claim_app_exit());
    }

    fn st(name: &str, state: ServiceState, win32: u32, specific: u32) -> ServiceStatus {
        ServiceStatus {
            name: name.to_string(),
            state,
            pid: matches!(state, ServiceState::Running).then_some(100),
            child_pid: matches!(state, ServiceState::Running).then_some(101),
            win32_exit_code: win32,
            service_specific_exit_code: specific,
        }
    }

    fn sup_row(id: &str, state: ProcessState, pid: Option<u32>, adopted: bool) -> ProcessStatus {
        ProcessStatus {
            id: id.to_string(),
            state,
            pid,
            adopted,
            restarts: 0,
            uptime: pid.map(|_| Duration::from_secs(30)),
            failure_reason: None,
        }
    }

    fn no_start(_: u32) -> Option<DateTime<Utc>> {
        None
    }

    #[test]
    fn try_begin_is_busy_while_held_and_drop_clears() {
        let reg = Arc::new(LifecycleRegistry::new());
        let g = reg
            .try_begin(GATEWAY, TransitionKind::Restarting, Origin::Activation)
            .unwrap();
        assert_eq!(
            reg.transition(GATEWAY).map(|t| t.kind),
            Some(TransitionKind::Restarting)
        );
        assert!(reg.any_transition());
        let busy = reg
            .try_begin(GATEWAY, TransitionKind::Stopping, Origin::Ui)
            .unwrap_err();
        assert_eq!(busy.kind, TransitionKind::Restarting);
        assert_eq!(busy.origin, Origin::Activation);
        let text = busy.to_string();
        assert!(
            text.starts_with("gateway restart already in progress (started by activation"),
            "{text}"
        );
        // Other keys are independent.
        let other = reg
            .try_begin(&backend_key("a"), TransitionKind::Starting, Origin::Ui)
            .unwrap();
        drop(g);
        assert!(reg.transition(GATEWAY).is_none());
        assert!(reg.record(GATEWAY).last_settled_at.is_some());
        assert!(reg.any_transition());
        drop(other);
        assert!(!reg.any_transition());
        // Released: a new operation may begin.
        assert!(reg
            .try_begin(GATEWAY, TransitionKind::Stopping, Origin::Ui)
            .is_ok());
    }

    #[test]
    fn stale_guard_does_not_clear_a_newer_transition() {
        let reg = Arc::new(LifecycleRegistry::new());
        let g = reg
            .try_begin(POSTGRES, TransitionKind::Starting, Origin::Wizard)
            .unwrap();
        let seq = g.seq();
        // Simulate a newer transition replacing the record (e.g. a
        // registry reset) while the old guard is still alive.
        reg.with(POSTGRES, |r| {
            r.transition = Some(Transition {
                kind: TransitionKind::Stopping,
                origin: Origin::Ui,
                since: Utc::now(),
                seq: seq + 100,
            })
        });
        drop(g);
        assert_eq!(
            reg.transition(POSTGRES).map(|t| t.kind),
            Some(TransitionKind::Stopping)
        );
    }

    #[test]
    fn generation_bumps_on_begin_and_intent() {
        let reg = Arc::new(LifecycleRegistry::new());
        assert_eq!(reg.generation(GATEWAY), 0);
        reg.set_intent(GATEWAY, Intent::Stopped);
        assert_eq!(reg.generation(GATEWAY), 1);
        assert_eq!(reg.intent(GATEWAY), Some(Intent::Stopped));
        let g = reg
            .try_begin(GATEWAY, TransitionKind::Starting, Origin::Ui)
            .unwrap();
        assert_eq!(reg.generation(GATEWAY), 2);
        drop(g);
        // Failure, readiness, observation and probe bookkeeping.
        reg.set_failure(GATEWAY, Failure::now("exited", Some(3)));
        assert_eq!(reg.record(GATEWAY).failure.unwrap().exit_code, Some(3));
        reg.clear_failure(GATEWAY);
        assert!(reg.record(GATEWAY).failure.is_none());
        reg.set_ready("backend:a", 42);
        assert_eq!(reg.record("backend:a").ready_pid, Some(42));
        assert_eq!(reg.observe(GATEWAY, LifecycleState::Running), None);
        assert_eq!(reg.observe(GATEWAY, LifecycleState::Running), None);
        assert_eq!(
            reg.observe(GATEWAY, LifecycleState::Failed),
            Some(LifecycleState::Running)
        );
        assert_eq!(reg.cached_probe(GATEWAY, Duration::from_secs(2)), None);
        reg.set_probe(GATEWAY, true);
        assert_eq!(reg.cached_probe(GATEWAY, Duration::from_secs(2)), Some(true));
        assert_eq!(reg.cached_probe(GATEWAY, Duration::ZERO), None);
    }

    #[test]
    fn merge_precedence_transition_over_scm_over_supervisor() {
        let configured = vec![("a".to_string(), 8081), ("b".to_string(), 8082)];
        let sup = vec![
            sup_row("a", ProcessState::Running, Some(10), false),
            sup_row("b", ProcessState::Stopped, None, false),
        ];
        let mut scm = HashMap::new();
        scm.insert(
            manager_services::backend_service_name("b"),
            st("llm-backend-b", ServiceState::Stopped, 1066, 1),
        );
        let mut records = HashMap::new();
        records.insert(
            backend_key("a"),
            ServiceRecord {
                transition: Some(Transition {
                    kind: TransitionKind::Restarting,
                    origin: Origin::Activation,
                    since: Utc::now(),
                    seq: 1,
                }),
                ready_pid: Some(10),
                ..Default::default()
            },
        );
        let now = Utc::now();
        let rows = merge_backend_liveness(&sup, &scm, &records, &configured, true, now, &no_start);
        // Transition wins over the supervisor's running.
        assert_eq!(rows["a"].state, LifecycleState::Restarting);
        assert!(rows["a"].transition.is_some());
        assert_eq!(rows["a"].uptime_secs, Some(30));
        // SCM Stopped with a service-specific code → failed with the
        // credential reason, service-managed.
        assert_eq!(rows["b"].state, LifecycleState::Failed);
        assert!(rows["b"].service_managed);
        assert!(rows["b"]
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("environment/credentials"));
        // Process mode: a stopped registration does not override the
        // supervisor (b stays plain stopped).
        let rows = merge_backend_liveness(&sup, &scm, &records, &configured, false, now, &no_start);
        assert_eq!(rows["b"].state, LifecycleState::Stopped);
        assert!(!rows["b"].service_managed);
    }

    #[test]
    fn merge_running_not_ready_reads_starting() {
        let configured = vec![
            ("a".to_string(), 8081),
            ("b".to_string(), 8082),
            ("c".to_string(), 8083),
            ("s".to_string(), 8084),
        ];
        let sup = vec![
            sup_row("a", ProcessState::Running, Some(10), false),
            sup_row("b", ProcessState::Running, Some(20), true),
            sup_row("c", ProcessState::Running, Some(30), false),
        ];
        let mut scm = HashMap::new();
        scm.insert(
            manager_services::backend_service_name("s"),
            st("llm-backend-s", ServiceState::Running, 0, 0),
        );
        let mut records = HashMap::new();
        records.insert(
            backend_key("c"),
            ServiceRecord {
                ready_pid: Some(30),
                ..Default::default()
            },
        );
        records.insert(
            backend_key("s"),
            ServiceRecord {
                ready_pid: Some(101),
                ..Default::default()
            },
        );
        let start = |_pid: u32| Some(Utc::now() - chrono::Duration::seconds(90));
        let rows = merge_backend_liveness(
            &sup,
            &scm,
            &records,
            &configured,
            true,
            Utc::now(),
            &start,
        );
        assert_eq!(rows["a"].state, LifecycleState::Starting, "not ready yet");
        assert_eq!(rows["b"].state, LifecycleState::Running, "adopted counts as ready");
        assert_eq!(rows["c"].state, LifecycleState::Running);
        // Service-managed: readiness is keyed on the child pid; the
        // start instant comes from the process start time.
        assert_eq!(rows["s"].state, LifecycleState::Running);
        assert_eq!(rows["s"].process_pid(), Some(101));
        assert!(rows["s"].uptime_secs.unwrap() >= 89);
    }

    #[test]
    fn merge_recorded_crash_and_orphans() {
        let configured = vec![("a".to_string(), 8081)];
        let sup = vec![
            sup_row("a", ProcessState::Stopped, None, false),
            sup_row("orphan", ProcessState::Running, Some(77), true),
        ];
        let mut records = HashMap::new();
        records.insert(
            backend_key("a"),
            ServiceRecord {
                intent: Some(Intent::Running),
                failure: Some(Failure::now("exited with code 3", Some(3))),
                ..Default::default()
            },
        );
        let rows = merge_backend_liveness(
            &sup,
            &HashMap::new(),
            &records,
            &configured,
            false,
            Utc::now(),
            &no_start,
        );
        assert_eq!(rows["a"].state, LifecycleState::Failed);
        assert_eq!(rows["a"].failure_reason.as_deref(), Some("exited with code 3"));
        assert_eq!(rows["orphan"].state, LifecycleState::Running);
        // An operator stop is never shown as failed.
        records.get_mut(&backend_key("a")).unwrap().intent = Some(Intent::Stopped);
        let rows = merge_backend_liveness(
            &sup,
            &HashMap::new(),
            &records,
            &configured,
            false,
            Utc::now(),
            &no_start,
        );
        assert_eq!(rows["a"].state, LifecycleState::Stopped);
    }

    #[test]
    fn merge_process_mode_uses_a_live_registered_service() {
        // Mixed: process mode, but the backend's service is running
        // (registered earlier) and the supervisor has nothing live.
        let configured = vec![("a".to_string(), 8081)];
        let mut scm = HashMap::new();
        scm.insert(
            manager_services::backend_service_name("a"),
            st("llm-backend-a", ServiceState::StartPending, 0, 0),
        );
        let rows = merge_backend_liveness(
            &[],
            &scm,
            &HashMap::new(),
            &configured,
            false,
            Utc::now(),
            &no_start,
        );
        assert_eq!(rows["a"].state, LifecycleState::Starting);
        assert!(rows["a"].service_managed);
    }

    #[test]
    fn derive_gateway_state_table() {
        use LifecycleState as L;
        let base = GatewayInputs::default();
        let cases: Vec<(GatewayInputs, LifecycleState)> = vec![
            (base, L::Stopped),
            (
                GatewayInputs {
                    transition: Some(TransitionKind::Restarting),
                    probe_up: true,
                    ..base
                },
                L::Restarting,
            ),
            (
                GatewayInputs {
                    service_route: true,
                    scm_state: Some(L::Stopping),
                    probe_up: true,
                    ..base
                },
                L::Stopping,
            ),
            (
                // SCM pending is ignored off the service route.
                GatewayInputs {
                    scm_state: Some(L::Starting),
                    probe_up: true,
                    ..base
                },
                L::Running,
            ),
            (GatewayInputs { probe_up: true, ..base }, L::Running),
            (
                GatewayInputs {
                    pid_alive: true,
                    recent_start: true,
                    ..base
                },
                L::Starting,
            ),
            (GatewayInputs { pid_alive: true, ..base }, L::Degraded),
            (
                // Service host running, gateway child not up yet or
                // being respawned: alive, never an operator stop.
                GatewayInputs {
                    service_route: true,
                    scm_state: Some(L::Running),
                    intent: Some(Intent::Running),
                    ..base
                },
                L::Degraded,
            ),
            (
                GatewayInputs {
                    service_route: true,
                    scm_state: Some(L::Running),
                    recent_start: true,
                    ..base
                },
                L::Starting,
            ),
            (
                // Off the service route the SCM row says nothing.
                GatewayInputs {
                    scm_state: Some(L::Running),
                    ..base
                },
                L::Stopped,
            ),
            (
                GatewayInputs {
                    intent: Some(Intent::Running),
                    has_failure: true,
                    ..base
                },
                L::Failed,
            ),
            (
                // An operator stop with an old failure reads stopped.
                GatewayInputs {
                    intent: Some(Intent::Stopped),
                    has_failure: true,
                    ..base
                },
                L::Stopped,
            ),
            (
                GatewayInputs {
                    service_route: true,
                    scm_state: Some(L::Failed),
                    ..base
                },
                L::Failed,
            ),
            (
                GatewayInputs {
                    service_route: true,
                    scm_state: Some(L::Failed),
                    intent: Some(Intent::Stopped),
                    ..base
                },
                L::Stopped,
            ),
        ];
        for (i, (inputs, want)) in cases.into_iter().enumerate() {
            assert_eq!(derive_gateway_state(inputs), want, "case {i}: {inputs:?}");
        }
    }

    #[test]
    fn settled_state_of_a_finished_operation() {
        use LifecycleState as L;
        assert_eq!(settled_state(TransitionKind::Stopping, None), L::Stopped);
        assert_eq!(
            settled_state(TransitionKind::Stopping, Some(L::Starting)),
            L::Stopped
        );
        assert_eq!(settled_state(TransitionKind::Starting, None), L::Running);
        // A backend start that did not wait for readiness stays starting.
        assert_eq!(
            settled_state(TransitionKind::Restarting, Some(L::Starting)),
            L::Starting
        );
    }

    #[test]
    fn app_exiting_is_claimed_once_and_readable() {
        let reg = LifecycleRegistry::new();
        assert!(!reg.app_exiting());
        assert!(reg.claim_app_exit());
        assert!(reg.app_exiting());
        assert!(!reg.claim_app_exit());
    }

    #[test]
    fn derive_postgres_and_receiver_states() {
        use LifecycleState as L;
        assert_eq!(derive_postgres_state(false, None, true, true, true, false), L::NotInstalled);
        assert_eq!(
            derive_postgres_state(true, Some(TransitionKind::Stopping), true, true, true, false),
            L::Stopping
        );
        assert_eq!(derive_postgres_state(true, None, true, true, false, false), L::Running);
        assert_eq!(derive_postgres_state(true, None, true, false, false, false), L::Starting);
        assert_eq!(derive_postgres_state(true, None, false, false, true, false), L::Failed);
        assert_eq!(derive_postgres_state(true, None, false, false, false, false), L::Stopped);
        // A live postmaster on the old port (configured port changed) is
        // running and awaiting a restart, never "starting" forever.
        assert_eq!(derive_postgres_state(true, None, true, false, true, true), L::Running);

        assert_eq!(derive_log_receiver_state(None, None, false), L::NotInstalled);
        let running = st("llm-log-receiver", ServiceState::Running, 0, 0);
        assert_eq!(derive_log_receiver_state(None, Some(&running), true), L::Running);
        assert_eq!(derive_log_receiver_state(None, Some(&running), false), L::Degraded);
        let crashed = st("llm-log-receiver", ServiceState::Stopped, 1066, 3);
        assert_eq!(derive_log_receiver_state(None, Some(&crashed), false), L::Failed);
        assert_eq!(scm_exit_code(&crashed), Some(3));
        assert_eq!(
            derive_log_receiver_state(Some(TransitionKind::Restarting), Some(&crashed), false),
            L::Restarting
        );
    }

    #[test]
    fn payload_failure_rule() {
        assert_eq!(payload_failure(&json!({ "ok": true })), None);
        assert_eq!(payload_failure(&json!({ "state": "done" })), None);
        assert_eq!(
            payload_failure(&json!({ "ok": false, "message": "PostgreSQL not ready" })),
            Some("PostgreSQL not ready".to_string())
        );
        assert_eq!(
            payload_failure(&json!({ "ok": false })),
            Some("the operation failed".to_string())
        );
        assert_eq!(
            payload_failure(&json!({ "busy": true, "running_step": "start-gateway" })),
            Some("busy: start-gateway is running".to_string())
        );
        assert_eq!(
            payload_failure(&json!({ "busy": true, "error": "gateway restart already in progress" })),
            Some("gateway restart already in progress".to_string())
        );
        assert_eq!(payload_failure(&json!({ "busy": false, "ok": true })), None);
    }

    #[test]
    fn keys_paths_and_event_payloads() {
        let dir = Path::new("/data");
        assert_eq!(backend_key("planner"), "backend:planner");
        assert_eq!(backend_id_of("backend:planner"), Some("planner"));
        assert_eq!(backend_id_of(GATEWAY), None);
        assert_eq!(
            log_path(dir, GATEWAY),
            dir.join("logs").join("gateway.lifecycle.log")
        );
        assert_eq!(
            log_path(dir, POSTGRES),
            dir.join("logs").join("postgres.lifecycle.log")
        );
        assert_eq!(
            log_path(dir, "backend:planner"),
            dir.join("logs").join("backend-planner.lifecycle.log")
        );
        assert_eq!(
            log_path(dir, LOG_RECEIVER),
            dir.join("log-receiver").join("logs").join("receiver.lifecycle.log")
        );
        let p = event_payload(
            "backend:planner",
            "restarting",
            LifecycleState::Restarting,
            Some(Origin::Activation),
            None,
        );
        assert_eq!(p["kind"], "backend");
        assert_eq!(p["id"], "planner");
        assert_eq!(p["state"], "restarting");
        assert_eq!(p["origin"], "activation");
        let g = event_payload(GATEWAY, "stopped", LifecycleState::Stopped, None, Some("pid 4"));
        assert_eq!(g["kind"], "gateway");
        assert!(g["id"].is_null());
        assert!(g["origin"].is_null());
    }

    /// Records every emitted event.
    #[derive(Default)]
    struct RecSink(Mutex<Vec<(String, Value)>>);
    impl EventSink for RecSink {
        fn emit(&self, event: &str, payload: Value) {
            lock(&self.0).push((event.to_string(), payload));
        }
    }

    #[tokio::test]
    async fn begin_and_finish_write_markers_and_events() {
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-lifecycle-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = manager_config::AppConfig::default_config();
        cfg.data_dir = dir.clone();
        let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
        let rec = Arc::new(RecSink::default());
        let sink: Arc<dyn EventSink> = rec.clone();
        let key = backend_key("planner");
        state.lifecycle.set_failure(&key, Failure::now("old crash", None));
        let guard = begin(
            &state,
            &sink,
            &key,
            TransitionKind::Restarting,
            Origin::Activation,
            None,
        )
        .await
        .unwrap();
        // The old failure stands until the operation succeeds.
        assert!(state.lifecycle.record(&key).failure.is_some());
        let busy = begin(&state, &sink, &key, TransitionKind::Stopping, Origin::Ui, None)
            .await
            .unwrap_err();
        assert!(matches!(busy, ApiError::Busy(_)), "{busy}");
        finish(&state, sink.as_ref(), guard, Ok("pid 42".to_string()));
        assert!(state.lifecycle.transition(&key).is_none());
        assert!(state.lifecycle.record(&key).failure.is_none());

        let guard = begin(&state, &sink, &key, TransitionKind::Starting, Origin::Ui, None)
            .await
            .unwrap();
        finish(&state, sink.as_ref(), guard, Err("port 8081 in use".to_string()));
        assert_eq!(
            state.lifecycle.record(&key).failure.map(|f| f.reason),
            Some("port 8081 in use".to_string())
        );

        let log = std::fs::read_to_string(log_path(&dir, &key)).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 4, "{log}");
        assert!(lines[0].ends_with("backend:planner: restart requested — by activation"));
        assert!(lines[1].ends_with("backend:planner: restarted: pid 42 — by activation"));
        assert!(lines[2].ends_with("backend:planner: start requested — by ui"));
        assert!(lines[3].ends_with("backend:planner: start failed: port 8081 in use — by ui"));

        let events = lock(&rec.0).clone();
        let lifecycle: Vec<&str> = events
            .iter()
            .filter(|(e, _)| e == events::SERVICE_LIFECYCLE)
            .map(|(_, p)| p["event"].as_str().unwrap())
            .collect();
        assert_eq!(lifecycle, ["restarting", "restarted", "starting", "failed"]);
        let backend: Vec<&Value> = events
            .iter()
            .filter(|(e, _)| e == events::BACKEND_STATUS)
            .map(|(_, p)| p)
            .collect();
        assert_eq!(backend.len(), 4);
        assert_eq!(backend[3]["error"], "port 8081 in use");
        assert_eq!(backend[3]["state"], "failed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
