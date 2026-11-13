//! The one lifecycle path for llama-server backends: start, stop and
//! restart, in both run modes, for every caller (the Backends UI, the
//! activation plan, REST, Bench and Optimize).
//!
//! Each primitive owns:
//! - route selection: the SCM when the backend's service is the active
//!   route (service mode + registered), otherwise the supervisor;
//! - the start preflight, the task record and the offload probe;
//! - the per-backend transition guard ([`lifecycle::begin`] /
//!   [`lifecycle::finish`]), so every status surface reads
//!   `starting` / `stopping` / `restarting` while it runs, and a second
//!   operation on the same backend is refused as busy;
//! - lifecycle markers in `logs/backend-<id>.lifecycle.log` (merged into
//!   the backend's log scope) and the SERVICE_LIFECYCLE / BACKEND_STATUS
//!   events;
//! - operator intent (start/restart → Running, stop → Stopped before the
//!   kill), so the monitor never reads an operator stop as a crash;
//! - lock-split supervisor calls: the supervisor mutex is held only for
//!   the short bookkeeping steps, never across a drain, a port wait or a
//!   spawn, so status reads stay live while an operation runs;
//! - readiness: a started backend reads `starting` until its `/health`
//!   answers 200 ([`wait_backend_ready`]).
//!
//! Temporary bench/optimize backends (`temp_id`s) are not managed
//! services and keep their direct supervisor use.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use manager_services::{ServiceStatus, ServiceTimeouts, StopOutcome};
use manager_supervisor::{HealthProbe, HealthStatus, HttpHealthProbe, SupervisorError};
use serde_json::{json, Value};

use crate::lifecycle::{self, backend_key, Intent, LifecycleState, Origin, TransitionKind};
use crate::{ApiError, AppState, EventSink, TaskStatus};

/// How long a start that does not wait for readiness keeps watching for
/// it in the background (model loads of large GGUFs take minutes).
const BACKGROUND_READY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a graceful stop waits for in-flight requests to finish
/// (both modes) before the stop proper.
const IDLE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Options of one backend lifecycle operation.
#[derive(Debug, Clone, Copy)]
pub struct BackendOpOpts {
    /// Stop: wait for in-flight requests (`/slots`) and drain instead of
    /// killing at once.
    pub graceful: bool,
    /// Who asked (markers, events, busy messages).
    pub origin: Origin,
    /// Start/restart: also wait (up to this long) for `/health` to
    /// answer 200 before reporting success. `None` returns at spawn and
    /// watches readiness in the background.
    pub wait_ready: Option<Duration>,
    /// Start/restart: the operator's WDDM over-commit confirmation
    /// (see `vram_start_gate`).
    pub allow_wddm_overcommit: bool,
    /// Record a task (`backend_start` / `backend_stop` /
    /// `backend_restart`) for the operation.
    pub record_task: bool,
    /// Start/restart: run the start preflight. False only when the
    /// caller already ran it.
    pub preflight: bool,
}

impl BackendOpOpts {
    /// Defaults for `origin`: immediate stop, no readiness wait, no
    /// over-commit, a task record, the preflight.
    pub fn new(origin: Origin) -> Self {
        Self {
            graceful: false,
            origin,
            wait_ready: None,
            allow_wddm_overcommit: false,
            record_task: true,
            preflight: true,
        }
    }
}

/// Which mechanism carries the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The backend's Windows service, through the SCM.
    Scm,
    /// The app's process supervisor.
    Process,
}

impl Route {
    fn service_managed(self) -> bool {
        self == Route::Scm
    }
}

fn pid_file(state: &AppState, id: &str) -> std::path::PathBuf {
    state.data_dir.join("logs").join(format!("backend-{id}.pid"))
}

/// Whether the supervisor holds a live process for `id`.
async fn supervisor_live(state: &AppState, id: &str) -> bool {
    let sup = state.supervisor().await.lock().await;
    sup.status().iter().any(|s| s.id == id && s.pid.is_some())
}

/// The route of a start: the SCM exactly when the service route is
/// active (service mode and registered).
async fn start_route(state: &AppState, id: &str) -> Route {
    if state
        .service_route_active(&manager_services::backend_service_name(id))
        .await
    {
        Route::Scm
    } else {
        Route::Process
    }
}

/// The route of a stop: the start route, plus a live registered service
/// the supervisor does not own (a mode flip, or a service left behind
/// by a deleted backend) — stop whatever is actually running.
async fn stop_route(state: &AppState, id: &str) -> Route {
    if start_route(state, id).await == Route::Scm {
        return Route::Scm;
    }
    let name = manager_services::backend_service_name(id);
    let live_service = lifecycle::scm_status(state, &name, Duration::from_secs(1))
        .await
        .is_some_and(|s| lifecycle::registered(&s) && lifecycle::scm_live(&s));
    if live_service && !supervisor_live(state, id).await {
        Route::Scm
    } else {
        Route::Process
    }
}

/// The backend's port: the configured one, else its spawn spec's.
async fn backend_port(state: &AppState, id: &str) -> Option<u16> {
    let configured = state
        .config
        .read()
        .await
        .backends
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.port);
    if configured.is_some() {
        return configured;
    }
    let sup = state.supervisor().await.lock().await;
    sup.spec(id).and_then(|s| s.port)
}

fn mark(state: &AppState, id: &str, event: &str, detail: Option<&str>, origin: Origin) {
    lifecycle::mark(&state.data_dir, &backend_key(id), event, detail, Some(origin));
}

// ---------------------------------------------------------------------------
// Primitives without a guard (the public operations compose them)
// ---------------------------------------------------------------------------

/// Bring `id` up through `route`. Returns the outcome detail and the
/// llama-server pid when known. Writes the "spawned" marker.
async fn spawn_inner(
    state: &Arc<AppState>,
    id: &str,
    route: Route,
    origin: Origin,
) -> Result<(String, Option<u32>), ApiError> {
    match route {
        Route::Scm => {
            state
                .write_backend_service_definition(id)
                .await
                .map_err(ApiError::Services)?;
            let name = manager_services::backend_service_name(id);
            let started = Instant::now();
            let status: ServiceStatus = tokio::task::spawn_blocking(move || {
                let control = manager_services::scm::platform_control();
                manager_services::start_and_wait(&control, &name, ServiceTimeouts::start(&name))
            })
            .await
            .map_err(|e| ApiError::Services(format!("service start task failed: {e}")))?
            .map_err(|e| ApiError::Services(e.to_string()))?;
            let pid = status
                .child_pid
                .or_else(|| {
                    lifecycle::read_pid_file(&pid_file(state, id))
                        .filter(|p| manager_supervisor::pid_alive(*p))
                })
                .or(status.pid);
            let detail = format!(
                "service running after {:.1} s{}",
                started.elapsed().as_secs_f64(),
                pid.map(|p| format!(", pid {p}")).unwrap_or_default()
            );
            mark(state, id, "spawned", Some(&detail), origin);
            Ok((detail, pid))
        }
        Route::Process => {
            let spec = {
                let mut sup = state.supervisor().await.lock().await;
                // Reap first: a backend that crashed since the last
                // monitor tick would otherwise read AlreadyRunning for
                // a corpse.
                sup.reap_exited();
                sup.prepare_spawn(id)
            }
            .map_err(ApiError::from)?;
            // The port wait and the spawn run with no supervisor lock.
            match manager_supervisor::spawn_process(id, &spec).await {
                Ok(proc) => {
                    let pid = proc.pid();
                    {
                        let mut sup = state.supervisor().await.lock().await;
                        sup.commit_spawn(id, proc)
                    }
                    .map_err(ApiError::from)?;
                    let detail = match pid {
                        Some(p) => format!("pid {p}"),
                        None => "pid unknown".to_string(),
                    };
                    mark(state, id, "spawned", Some(&detail), origin);
                    Ok((detail, pid))
                }
                Err(e) => {
                    let reason = format!("spawn failed: {e}");
                    {
                        let mut sup = state.supervisor().await.lock().await;
                        sup.record_spawn_failure(id, reason);
                    }
                    Err(ApiError::from(e))
                }
            }
        }
    }
}

/// Take `id` down through `route`. Returns the outcome detail. A
/// backend that is not running is a successful no-op ("was not
/// running"); an id with neither a spec nor a service is NotFound.
async fn stop_inner(
    state: &Arc<AppState>,
    id: &str,
    route: Route,
    graceful: bool,
    origin: Origin,
) -> Result<String, ApiError> {
    if graceful {
        if let Some(port) = backend_port(state, id).await {
            let started = Instant::now();
            if !wait_backend_idle(port, IDLE_DRAIN_TIMEOUT).await {
                mark(
                    state,
                    id,
                    "drain timed out",
                    Some(&format!(
                        "requests still in flight after {} s; stopping anyway",
                        started.elapsed().as_secs()
                    )),
                    origin,
                );
            }
        }
    }
    match route {
        Route::Scm => {
            let name = manager_services::backend_service_name(id);
            let outcome = tokio::task::spawn_blocking(move || {
                let control = manager_services::scm::platform_control();
                manager_services::stop_and_wait(&control, &name, ServiceTimeouts::stop(&name))
            })
            .await
            .map_err(|e| ApiError::Services(format!("service stop task failed: {e}")))?
            .map_err(|e| ApiError::Services(e.to_string()))?;
            Ok(match outcome {
                StopOutcome::NotRegistered => "service not registered".to_string(),
                StopOutcome::AlreadyStopped => "was not running".to_string(),
                StopOutcome::Stopped { waited_ms } => {
                    format!("service stopped after {:.1} s", waited_ms as f64 / 1000.0)
                }
            })
        }
        Route::Process => {
            let detached = {
                let mut sup = state.supervisor().await.lock().await;
                if sup.spec(id).is_none() {
                    return Err(ApiError::NotFound(format!("backend {id}")));
                }
                sup.detach(id)
            };
            let mut proc = match detached {
                Ok(p) => p,
                Err(SupervisorError::NotFound(_)) => return Ok("was not running".to_string()),
                Err(e) => return Err(e.into()),
            };
            let pid = proc.pid();
            let started = Instant::now();
            // Drain or kill with no supervisor lock held.
            let result = manager_supervisor::stop_process(&mut proc, graceful).await;
            {
                let mut sup = state.supervisor().await.lock().await;
                sup.finish_stop(id, proc, &result);
            }
            result.map_err(ApiError::from)?;
            Ok(format!(
                "{} pid {} after {:.1} s",
                if graceful { "drained" } else { "killed" },
                pid.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string()),
                started.elapsed().as_secs_f64()
            ))
        }
    }
}

/// Wait until the backend at `port` has no slot processing a request,
/// up to `timeout`. True when idle (or when `/slots` is unavailable:
/// nothing to wait for), false on timeout.
async fn wait_backend_idle(port: u16, timeout: Duration) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    else {
        return true;
    };
    let url = format!("http://127.0.0.1:{port}/slots");
    let deadline = Instant::now() + timeout;
    loop {
        let busy = match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => resp
                .bytes()
                .await
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| slots_busy(&v)),
            _ => false,
        };
        if !busy {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Whether a llama-server `/slots` payload shows a slot processing a
/// request.
pub(crate) fn slots_busy(slots: &Value) -> bool {
    slots.as_array().is_some_and(|a| {
        a.iter()
            .any(|s| s.get("is_processing").and_then(Value::as_bool) == Some(true))
    })
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/// Why a readiness wait ended without a 200.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadyError {
    /// The process went away (or was replaced) first.
    Gone(String),
    /// The wait ran out.
    Timeout(Duration),
}

impl std::fmt::Display for ReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadyError::Gone(reason) => f.write_str(reason),
            ReadyError::Timeout(t) => write!(f, "did not become ready within {} s", t.as_secs()),
        }
    }
}

/// The one readiness poll: GET `http://127.0.0.1:{port}/health` (1 s
/// timeout) every `interval` until it answers 2xx. Before each probe
/// `gone` is asked whether the process is still there; `Some(reason)`
/// ends the wait early.
pub(crate) async fn poll_health_until<F, Fut>(
    port: u16,
    timeout: Duration,
    interval: Duration,
    mut gone: F,
) -> Result<(), ReadyError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let probe = HttpHealthProbe::new(
        format!("http://127.0.0.1:{port}/health"),
        Duration::from_secs(1),
    );
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(reason) = gone().await {
            return Err(ReadyError::Gone(reason));
        }
        if let Ok(HealthStatus::Healthy) = probe.probe().await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ReadyError::Timeout(timeout));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Whether a readiness wait must give up on `row` (pure). `expected` is
/// the supervisor/service pid first observed; a different pid means the
/// process was replaced. Returns the pid to keep tracking.
pub(crate) fn readiness_gone(
    row: Option<&lifecycle::BackendLive>,
    expected: Option<u32>,
) -> Result<u32, String> {
    let Some(row) = row else {
        return Err("the backend is no longer listed".to_string());
    };
    if row
        .transition
        .as_ref()
        .is_some_and(|t| t.kind == TransitionKind::Stopping)
    {
        return Err("stopped before it became ready".to_string());
    }
    let Some(pid) = row.pid else {
        return Err(match &row.failure_reason {
            Some(reason) => format!("exited before it became ready: {reason}"),
            None => "exited before it became ready".to_string(),
        });
    };
    if let Some(e) = expected {
        if e != pid {
            return Err(format!(
                "process replaced (pid {e} -> {pid}) before it became ready"
            ));
        }
    }
    Ok(pid)
}

/// Wait until backend `id` answers `/health` with 200, in either mode.
/// Ends early when its process exits, is stopped or is replaced (the
/// first observed pid is the one waited on). On success: records
/// readiness for the llama-server pid (the row reads `running` from
/// then on), writes the "ready" marker ("after X s") and emits `ready`.
pub async fn wait_backend_ready(
    state: &AppState,
    sink: &dyn EventSink,
    id: &str,
    timeout: Duration,
    origin: Origin,
) -> Result<(), String> {
    let port = backend_port(state, id)
        .await
        .ok_or_else(|| format!("backend {id} has no port"))?;
    let started = Instant::now();
    let tracked: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);
    let result = poll_health_until(port, timeout, Duration::from_secs(1), || {
        let tracked = &tracked;
        async move {
            let live = lifecycle::backend_liveness(state).await;
            let expected = *tracked.lock().unwrap_or_else(|p| p.into_inner());
            match readiness_gone(live.get(id), expected) {
                Ok(pid) => {
                    if !manager_supervisor::pid_alive(pid) {
                        return Some(format!("pid {pid} exited before it became ready"));
                    }
                    *tracked.lock().unwrap_or_else(|p| p.into_inner()) = Some(pid);
                    None
                }
                Err(reason) => Some(reason),
            }
        }
    })
    .await;
    result.map_err(|e| e.to_string())?;
    // Readiness is recorded for the llama-server process itself (the
    // child under a service host), the pid the liveness merge compares.
    let live = lifecycle::backend_liveness(state).await;
    let tracked_pid = *tracked.lock().unwrap_or_else(|p| p.into_inner());
    let pid = live.get(id).and_then(|r| r.process_pid()).or(tracked_pid);
    let key = backend_key(id);
    if let Some(pid) = pid {
        if !state.lifecycle.set_ready(&key, pid) {
            // The lifecycle monitor's readiness probe got there first
            // and already wrote the marker and the event.
            return Ok(());
        }
    }
    let detail = format!(
        "after {:.1} s{}",
        started.elapsed().as_secs_f64(),
        pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
    );
    lifecycle::mark(&state.data_dir, &key, "ready", Some(&detail), Some(origin));
    lifecycle::emit(
        sink,
        &key,
        "ready",
        LifecycleState::Running,
        Some(origin),
        Some(&detail),
    );
    Ok(())
}

/// Watch readiness of a start that did not wait for it, so the row moves
/// from `starting` to `running` once the model has loaded.
fn watch_readiness(state: &Arc<AppState>, sink: &Arc<dyn EventSink>, id: &str, origin: Origin) {
    let state = Arc::clone(state);
    let sink = Arc::clone(sink);
    let id = id.to_string();
    tokio::spawn(async move {
        // A wait that ends early (stopped, crashed, replaced) is the
        // monitor's or the next operation's to report.
        let _ = wait_backend_ready(&state, sink.as_ref(), &id, BACKGROUND_READY_TIMEOUT, origin)
            .await;
    });
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

async fn begin_task(
    state: &AppState,
    opts: &BackendOpOpts,
    kind: &str,
    detail: Value,
) -> Option<String> {
    if opts.record_task {
        Some(state.record_task(kind, detail).await)
    } else {
        None
    }
}

async fn end_task(state: &AppState, task_id: &Option<String>, status: TaskStatus, detail: Value) {
    if let Some(task_id) = task_id {
        state.finish_task(task_id, status, detail).await;
    }
}

/// Begin the transition; a busy refusal also fails the task record.
async fn begin_op(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
    kind: TransitionKind,
    opts: &BackendOpOpts,
    task_id: &Option<String>,
) -> Result<lifecycle::TransitionGuard, ApiError> {
    let detail = (opts.graceful && kind != TransitionKind::Starting).then_some("graceful");
    match lifecycle::begin(state, sink, &backend_key(id), kind, opts.origin, detail).await {
        Ok(guard) => Ok(guard),
        Err(e) => {
            end_task(
                state,
                task_id,
                TaskStatus::Failed,
                json!({ "backend_id": id, "error": e.to_string() }),
            )
            .await;
            Err(e)
        }
    }
}

/// Wait for readiness inside the guard when the options ask for it.
async fn maybe_wait_ready(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
    opts: &BackendOpOpts,
) -> Result<(), ApiError> {
    match opts.wait_ready {
        Some(timeout) => wait_backend_ready(state, sink.as_ref(), id, timeout, opts.origin)
            .await
            .map_err(|e| ApiError::Supervisor(format!("backend {id} {e}"))),
        None => Ok(()),
    }
}

/// What a start or restart produced: the outcome detail, the pid and
/// the route it took.
type StartOutcome = Result<(String, Option<u32>, Route), ApiError>;

/// The common tail of start and restart: end the transition with the
/// outcome, end the task, start the offload probe and (when the caller
/// did not wait) the readiness watch.
#[allow(clippy::too_many_arguments)]
async fn conclude_start(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    guard: lifecycle::TransitionGuard,
    id: &str,
    task_id: Option<String>,
    probe_ctx: Option<(u32, u32, u16)>,
    opts: &BackendOpOpts,
    outcome: StartOutcome,
) -> Result<Value, ApiError> {
    match outcome {
        Ok((detail, pid, route)) => {
            // Not waited for readiness: the model is still loading, so
            // the start settles `starting`; the "ready" event (watcher or
            // monitor) moves it to `running`.
            let ready_state = opts.wait_ready.is_none().then_some(LifecycleState::Starting);
            lifecycle::finish_with(state, sink.as_ref(), guard, Ok(detail), ready_state);
            end_task(state, &task_id, TaskStatus::Done, json!({ "backend_id": id })).await;
            if let Some((layers, moe_layers, port)) = probe_ctx {
                crate::spawn_offload_probe(sink, state, id, layers, moe_layers, port);
            }
            if opts.wait_ready.is_none() {
                watch_readiness(state, sink, id, opts.origin);
            }
            Ok(json!({
                "task_id": task_id,
                "backend_id": id,
                "pid": pid,
                "service_managed": route.service_managed(),
                "ready": opts.wait_ready.is_some(),
            }))
        }
        Err(e) => {
            lifecycle::finish(state, sink.as_ref(), guard, Err(e.to_string()));
            end_task(
                state,
                &task_id,
                TaskStatus::Failed,
                json!({ "backend_id": id, "error": e.to_string() }),
            )
            .await;
            Err(e)
        }
    }
}

/// Start backend `id` (supervisor spawn, or SCM start when its service
/// is the active route). See the module docs for what it owns.
pub async fn start_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
    opts: BackendOpOpts,
) -> Result<Value, ApiError> {
    crate::ensure_backend_specs(state).await;
    if opts.preflight {
        crate::backend_start_preflight(state, id, opts.allow_wddm_overcommit).await?;
    }
    // Offload-probe capture: pending change + saved position, before
    // the spawn (a refused start never reaches here).
    let probe_ctx = crate::offload_probe_context(state, id).await;
    let task_id = begin_task(state, &opts, "backend_start", json!({ "backend_id": id })).await;
    let guard = begin_op(state, sink, id, TransitionKind::Starting, &opts, &task_id).await?;
    state.lifecycle.set_intent(&backend_key(id), Intent::Running);
    let outcome: StartOutcome = async {
        let route = start_route(state, id).await;
        let (detail, pid) = spawn_inner(state, id, route, opts.origin).await?;
        maybe_wait_ready(state, sink, id, &opts).await?;
        Ok((detail, pid, route))
    }
    .await;
    conclude_start(state, sink, guard, id, task_id, probe_ctx, &opts, outcome).await
}

/// Stop backend `id` (drain or kill under the supervisor, or SCM stop).
/// Intent becomes Stopped before anything is touched, so the exit is
/// never reported as a crash. Stopping a backend that is not running
/// succeeds ("was not running").
pub async fn stop_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
    opts: BackendOpOpts,
) -> Result<Value, ApiError> {
    crate::ensure_backend_specs(state).await;
    let task_id = begin_task(
        state,
        &opts,
        "backend_stop",
        json!({ "backend_id": id, "graceful": opts.graceful }),
    )
    .await;
    let guard = begin_op(state, sink, id, TransitionKind::Stopping, &opts, &task_id).await?;
    state.lifecycle.set_intent(&backend_key(id), Intent::Stopped);
    let route = stop_route(state, id).await;
    match stop_inner(state, id, route, opts.graceful, opts.origin).await {
        Ok(detail) => {
            lifecycle::finish(state, sink.as_ref(), guard, Ok(detail));
            end_task(state, &task_id, TaskStatus::Done, json!({ "backend_id": id })).await;
            Ok(json!({
                "task_id": task_id,
                "backend_id": id,
                "service_managed": route.service_managed(),
            }))
        }
        Err(e) => {
            lifecycle::finish(state, sink.as_ref(), guard, Err(e.to_string()));
            end_task(
                state,
                &task_id,
                TaskStatus::Failed,
                json!({ "backend_id": id, "error": e.to_string() }),
            )
            .await;
            Err(e)
        }
    }
}

/// Restart backend `id`: the stop and the start run inside one
/// `restarting` transition (every status surface reads `restarting`
/// throughout, never `stopped`), intent stays Running, and with
/// `wait_ready` the operation returns only once `/health` answers.
pub async fn restart_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
    opts: BackendOpOpts,
) -> Result<Value, ApiError> {
    crate::ensure_backend_specs(state).await;
    if opts.preflight {
        crate::backend_start_preflight(state, id, opts.allow_wddm_overcommit).await?;
    }
    let probe_ctx = crate::offload_probe_context(state, id).await;
    let task_id = begin_task(
        state,
        &opts,
        "backend_restart",
        json!({ "backend_id": id, "graceful": opts.graceful }),
    )
    .await;
    let guard = begin_op(state, sink, id, TransitionKind::Restarting, &opts, &task_id).await?;
    state.lifecycle.set_intent(&backend_key(id), Intent::Running);
    let outcome: StartOutcome = async {
        let route = stop_route(state, id).await;
        let stopped = stop_inner(state, id, route, opts.graceful, opts.origin).await?;
        mark(state, id, "stopped", Some(&stopped), opts.origin);
        let route = start_route(state, id).await;
        let (detail, pid) = spawn_inner(state, id, route, opts.origin).await?;
        maybe_wait_ready(state, sink, id, &opts).await?;
        Ok((detail, pid, route))
    }
    .await;
    conclude_start(state, sink, guard, id, task_id, probe_ctx, &opts, outcome).await
}

#[cfg(test)]
mod tests;
