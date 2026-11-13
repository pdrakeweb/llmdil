//! The one down-detection monitor for every managed service (design
//! component F): llama-server backends in both run modes, the gateway,
//! the app-managed PostgreSQL server and the log receiver.
//!
//! Each [`tick`] (the shell runs it every 3 s through
//! `notify::monitor_tick`):
//! 1. reaps exited supervised processes under a short supervisor lock and
//!    records each backend exit as its failure, with the
//!    "exited unexpectedly (<status>)" marker;
//! 2. probes `/health` of running backends not yet ready (no transition),
//!    so a row moves from `starting` to `running`;
//! 3. diffs every backend's state against the last observation and tells
//!    a crash from an operator stop with [`detect`] (intent, transition);
//! 4. checks the gateway (pid liveness + probe), skipped while a gateway
//!    operation is in flight and discarded when the lifecycle generation
//!    moved on during the probe (GW-5, GW-6, OS-7);
//! 5. checks PostgreSQL while the gateway is expected up;
//! 6. checks the log receiver;
//! 7. writes markers into each service's own lifecycle log and emits
//!    SERVICE_LIFECYCLE for every observed change.
//!
//! It returns [`MonitorEvent`]s; `notify::monitor_tick` turns them into
//! toasts. Policy (state model §5): nothing is restarted automatically.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use manager_supervisor::{HealthProbe, HealthStatus, HttpHealthProbe};

use crate::lifecycle::{
    self, backend_key, Failure, Intent, LifecycleState, Origin, GATEWAY, LOG_RECEIVER, POSTGRES,
};
use crate::{AppState, EventSink};

/// One state-driven event the notification layer reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorEvent {
    /// A backend stopped unexpectedly. `exit_status` is the reaped exit
    /// string (or the SCM exit code), `reason` the classified cause.
    Crashed {
        id: String,
        exit_status: Option<String>,
        reason: Option<String>,
    },
    /// The gateway the operator expects up went down.
    GatewayDown { reason: String },
    /// PostgreSQL went down while the gateway is expected up.
    PostgresDown { reason: String },
    /// The log receiver service stopped with an error.
    LogReceiverDown {
        exit_code: Option<i64>,
        reason: Option<String>,
    },
}

/// What [`detect`] found in one observed change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// A live service went down without being asked to.
    Crashed,
    /// A failed service is live again without an app operation (the SCM
    /// or an operator outside the app brought it back).
    Recovered,
}

/// The monitor's own counters, kept between ticks on `AppState`.
#[derive(Debug, Default)]
pub struct MonitorState {
    /// Consecutive ticks the gateway process was alive but not answering
    /// past its start grace.
    gateway_strikes: u8,
    /// Consecutive ticks PostgreSQL read failed while expected up.
    postgres_strikes: u8,
}

/// Consecutive bad observations before a "not answering" gateway or a
/// failed PostgreSQL is reported (a single 500 ms probe miss on a loaded
/// machine must not cry wolf).
const STRIKES: u8 = 2;

/// Readiness probes run at most this many at a time.
const READY_PROBE_PARALLEL: usize = 4;
const READY_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// A state the service was really up in (a transition state is the
/// operation's to report, never a crash baseline).
fn was_up(s: LifecycleState) -> bool {
    matches!(
        s,
        LifecycleState::Running | LifecycleState::Starting | LifecycleState::Degraded
    )
}

/// Classify one observed change (pure). `prev` is the previous
/// observation when the state changed (`None`: first observation or no
/// change). A crash is a move from up to stopped/failed with no
/// transition in flight while the operator wants the service running —
/// or, with no recorded intent at all, a move into `failed` (an exit the
/// supervisor or the SCM reported as an error is never an operator stop).
/// An operator stop (intent Stopped) and anything during a transition is
/// never a crash.
pub fn detect(
    prev: Option<LifecycleState>,
    now: LifecycleState,
    intent: Option<Intent>,
    transition: bool,
) -> Option<Detected> {
    let prev = prev?;
    if transition {
        return None;
    }
    if was_up(prev) {
        let crashed = match intent {
            Some(Intent::Running) => {
                matches!(now, LifecycleState::Stopped | LifecycleState::Failed)
            }
            None => now == LifecycleState::Failed,
            Some(Intent::Stopped) => false,
        };
        return crashed.then_some(Detected::Crashed);
    }
    if prev == LifecycleState::Failed
        && matches!(now, LifecycleState::Running | LifecycleState::Starting)
    {
        return Some(Detected::Recovered);
    }
    None
}

/// A service-managed instance that moved from up to a CLEAN stop with no
/// app operation in flight was stopped outside the app (services.msc,
/// `sc stop`): the shim reports exit 0 for a requested stop, so this is
/// an operator stop, not a crash (pure).
pub fn stopped_outside_app(
    service_managed: bool,
    prev: Option<LifecycleState>,
    now: LifecycleState,
    transition: bool,
) -> bool {
    service_managed && !transition && prev.is_some_and(was_up) && now == LifecycleState::Stopped
}

/// A service the operator had stopped (intent Stopped) that is live
/// again with no app operation in flight, after an observation that was
/// down, was started outside the app (services.msc, `sc start`, SCM
/// recovery, an elevated repair): it is expected up again, so a later
/// crash is reported instead of read as the old operator stop (pure).
pub fn started_outside_app(
    intent: Option<Intent>,
    prev: Option<LifecycleState>,
    now: LifecycleState,
    transition: bool,
) -> bool {
    !transition
        && intent == Some(Intent::Stopped)
        && prev.is_some_and(|p| matches!(p, LifecycleState::Stopped | LifecycleState::Failed))
        && was_up(now)
}

/// The SERVICE_LIFECYCLE `event` of a change the monitor observed.
pub fn observed_event(prev: Option<LifecycleState>, now: LifecycleState) -> &'static str {
    match now {
        LifecycleState::Running if prev == Some(LifecycleState::Starting) => "ready",
        LifecycleState::Running => "started",
        LifecycleState::Starting => "starting",
        LifecycleState::Restarting => "restarting",
        LifecycleState::Stopping => "stopping",
        LifecycleState::Stopped => "stopped",
        LifecycleState::Failed => "failed",
        LifecycleState::Degraded => "degraded",
        LifecycleState::Unknown | LifecycleState::NotInstalled => "observed",
    }
}

/// The failure text of a backend crash (pure): the exit and the
/// classified cause, as far as either is known.
pub fn crash_reason(exit_status: Option<&str>, reason: Option<&str>) -> String {
    match (exit_status, reason) {
        (Some(s), Some(r)) => format!("exited unexpectedly ({s}). {r}"),
        (Some(s), None) => format!("exited unexpectedly ({s})"),
        (None, Some(r)) => r.to_string(),
        (None, None) => "is no longer running".to_string(),
    }
}

/// Count one more bad (or a good) observation; returns the new count and
/// whether this observation reached the threshold (pure).
pub fn strike(bad: bool, strikes: u8) -> (u8, bool) {
    if !bad {
        return (0, false);
    }
    let n = strikes.saturating_add(1);
    (n, n == STRIKES)
}

/// The inputs of [`gateway_action`].
#[derive(Debug, Clone, Default)]
pub struct GatewayObs {
    pub state: Option<LifecycleState>,
    /// The previous tick's observation (the record's `last_observed`).
    pub prev: Option<LifecycleState>,
    pub up: bool,
    pub pid: Option<u32>,
    pub intent: Option<Intent>,
    /// A failure is already recorded (the down was reported).
    pub failure_recorded: bool,
    pub service_managed: bool,
    /// The SCM-reported failure, when the service route has one.
    pub scm_failure: Option<String>,
}

/// What the monitor does about the gateway this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayAction {
    Nothing,
    /// Up with no intent recorded: it is now expected up.
    SeedRunning,
    /// Up: reset the strike count (and clear a stale failure).
    Healthy,
    /// Alive but not answering past the start grace: one more strike.
    Strike,
    /// Expected up and gone: report it.
    Down(String),
    /// The Windows service was stopped cleanly outside the app.
    StoppedOutside,
    /// Stopped by the operator, now live again without an app operation:
    /// started outside the app, so it is expected up again.
    StartedOutside,
}

/// Decide the gateway's action (pure). Down is reported once (a recorded
/// failure latches it until the gateway answers again or the next start).
pub fn gateway_action(o: &GatewayObs, strikes: u8) -> GatewayAction {
    let now = if o.up {
        LifecycleState::Running
    } else {
        o.state.unwrap_or(LifecycleState::Unknown)
    };
    if started_outside_app(o.intent, o.prev, now, false) {
        return GatewayAction::StartedOutside;
    }
    if o.up {
        return if o.intent.is_none() {
            GatewayAction::SeedRunning
        } else {
            GatewayAction::Healthy
        };
    }
    if o.intent != Some(Intent::Running) || o.failure_recorded {
        return GatewayAction::Nothing;
    }
    if o.service_managed && o.state == Some(LifecycleState::Stopped) && o.scm_failure.is_none() {
        return GatewayAction::StoppedOutside;
    }
    // Something is alive: a gateway pid, or (on the service route) a
    // starting / running service host whose child has no pid yet.
    let alive = o.pid.is_some()
        || matches!(
            o.state,
            Some(LifecycleState::Starting | LifecycleState::Degraded)
        );
    if !alive {
        return GatewayAction::Down(
            o.scm_failure
                .clone()
                .unwrap_or_else(|| "process exited".to_string()),
        );
    }
    if o.state != Some(LifecycleState::Degraded) {
        return GatewayAction::Nothing;
    }
    let (_, fire) = strike(true, strikes);
    if !fire {
        return GatewayAction::Strike;
    }
    GatewayAction::Down(match o.pid {
        Some(pid) => format!("process {pid} is not answering"),
        None => "the gateway service is running but the gateway is not answering".to_string(),
    })
}

/// Whether a gateway observation taken under generation `before` may be
/// acted on (pure): an operator stop or a new operation that began while
/// the probe ran (generation moved, or a transition is now in flight)
/// makes it stale, so a stale "up" cannot re-arm the expectation and a
/// stale "down" cannot report a crash (GW-6).
pub fn result_is_current(before: u64, after: u64, transition: bool) -> bool {
    before == after && !transition
}

fn monitor_state(state: &AppState) -> std::sync::MutexGuard<'_, MonitorState> {
    state
        .lifecycle_monitor
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Record a crash: the failure (unless one is recorded already), the
/// "crashed" marker (unless `marked`), and the "crashed" event.
fn record_crash(
    state: &AppState,
    sink: &dyn EventSink,
    key: &str,
    reason: &str,
    exit_code: Option<i64>,
    marked: bool,
) {
    if state.lifecycle.record(key).failure.is_none() {
        state
            .lifecycle
            .set_failure(key, Failure::now(reason.to_string(), exit_code));
    }
    if !marked {
        lifecycle::mark(
            &state.data_dir,
            key,
            "crashed",
            Some(reason),
            Some(Origin::Monitor),
        );
    }
    lifecycle::emit(
        sink,
        key,
        "crashed",
        LifecycleState::Failed,
        Some(Origin::Monitor),
        Some(reason),
    );
}

/// Report a service stopped outside the app: intent Stopped, a marker and
/// the event, no toast.
fn record_outside_stop(state: &AppState, sink: &dyn EventSink, key: &str) {
    state.lifecycle.set_intent(key, Intent::Stopped);
    lifecycle::mark(
        &state.data_dir,
        key,
        "stopped",
        Some("outside the app"),
        Some(Origin::Scm),
    );
    lifecycle::emit(
        sink,
        key,
        "stopped",
        LifecycleState::Stopped,
        Some(Origin::Scm),
        Some("outside the app"),
    );
}

/// Report a service started outside the app (see
/// [`started_outside_app`]): intent Running again, the stale failure
/// cleared, a "started: outside the app" marker and the event, no toast.
fn record_outside_start(
    state: &AppState,
    sink: &dyn EventSink,
    key: &str,
    now: LifecycleState,
    service_managed: bool,
) {
    let origin = if service_managed {
        Origin::Scm
    } else {
        Origin::Monitor
    };
    state.lifecycle.set_intent(key, Intent::Running);
    state.lifecycle.clear_failure(key);
    lifecycle::mark(
        &state.data_dir,
        key,
        "started",
        Some("outside the app"),
        Some(origin),
    );
    lifecycle::emit(
        sink,
        key,
        observed_event(Some(LifecycleState::Stopped), now),
        now,
        Some(origin),
        Some("outside the app"),
    );
}

/// Report a recovery: the failure is cleared, with a marker and event.
fn record_recovery(state: &AppState, sink: &dyn EventSink, key: &str, now: LifecycleState) {
    state.lifecycle.clear_failure(key);
    lifecycle::mark(
        &state.data_dir,
        key,
        "recovered",
        Some(&format!("now {}", now.as_str())),
        Some(Origin::Monitor),
    );
    lifecycle::emit(
        sink,
        key,
        observed_event(Some(LifecycleState::Failed), now),
        now,
        Some(Origin::Monitor),
        Some("recovered"),
    );
}

/// Emit an observed change that is neither a crash nor a recovery.
fn emit_change(
    sink: &dyn EventSink,
    key: &str,
    prev: Option<LifecycleState>,
    now: LifecycleState,
    transition: bool,
) {
    if prev.is_some() && !transition {
        lifecycle::emit(
            sink,
            key,
            observed_event(prev, now),
            now,
            Some(Origin::Monitor),
            None,
        );
    }
}

/// One monitor pass. See the module docs for the order of the checks.
pub async fn tick(state: &AppState, sink: &dyn EventSink) -> Vec<MonitorEvent> {
    let mut events = Vec::new();
    // The app is exiting and has released its backends on purpose (they
    // are left running): nothing observed now is a crash.
    if state.lifecycle.app_exiting() {
        return events;
    }
    backends(state, sink, &mut events).await;
    gateway(state, sink, &mut events).await;
    postgres(state, sink, &mut events).await;
    log_receiver(state, sink, &mut events).await;
    events
}

/// One exit reaped this tick: backend id, exit status text, exit code.
type Reaped = (String, String, Option<i64>);

/// Steps 1–3 for every backend.
async fn backends(state: &AppState, sink: &dyn EventSink, events: &mut Vec<MonitorEvent>) {
    let owned: HashSet<String> = state
        .backend_spec_ids
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    // 1. Reap under a short supervisor lock, keeping the exit statuses
    //    (also of exits another reader reaped first).
    let (reaped, exits, fatal): (
        Vec<Reaped>,
        HashMap<String, String>,
        HashMap<String, String>,
    ) = {
        let mut sup = state.supervisor().await.lock().await;
        let reaped = sup
            .reap_exited()
            .into_iter()
            .filter(|(id, _)| owned.contains(id))
            .map(|(id, s)| (id, s.to_string(), s.code().map(i64::from)))
            .collect();
        let mut exits = HashMap::new();
        let mut fatal = HashMap::new();
        for st in sup.status() {
            if !owned.contains(&st.id) {
                continue;
            }
            if let Some(s) = sup.last_exit_status(&st.id) {
                exits.insert(st.id.clone(), s.to_string());
            }
            if let Some(r) = st.failure_reason {
                fatal.insert(st.id.clone(), r);
            }
        }
        (reaped, exits, fatal)
    };
    let mut marked: HashSet<String> = HashSet::new();
    for (id, status, code) in &reaped {
        let key = backend_key(id);
        let reason = crash_reason(Some(status), fatal.get(id).map(String::as_str));
        state
            .lifecycle
            .set_failure(&key, Failure::now(reason.clone(), *code));
        lifecycle::mark(
            &state.data_dir,
            &key,
            &format!("exited unexpectedly ({status})"),
            fatal.get(id).map(String::as_str),
            Some(Origin::Monitor),
        );
        marked.insert(key);
    }

    let mut rows = lifecycle::backend_liveness(state).await;

    // 2. Readiness of running rows still loading their model.
    let ports: HashMap<String, u16> = state
        .config
        .read()
        .await
        .backends
        .iter()
        .map(|b| (b.id.clone(), b.port))
        .collect();
    let records = state.lifecycle.records();
    let pending: Vec<(String, u32, u16)> = rows
        .values()
        .filter(|r| r.state == LifecycleState::Starting && r.transition.is_none())
        .filter_map(|r| {
            let pid = r.process_pid()?;
            let ready = records
                .get(&backend_key(&r.id))
                .and_then(|rec| rec.ready_pid);
            (ready != Some(pid)).then_some(())?;
            Some((r.id.clone(), pid, *ports.get(&r.id)?))
        })
        .collect();
    for chunk in pending.chunks(READY_PROBE_PARALLEL) {
        let mut set = tokio::task::JoinSet::new();
        for (id, pid, port) in chunk.iter().cloned() {
            set.spawn(async move {
                let probe = HttpHealthProbe::new(
                    format!("http://127.0.0.1:{port}/health"),
                    READY_PROBE_TIMEOUT,
                );
                let ok = matches!(probe.probe().await, Ok(HealthStatus::Healthy));
                (id, pid, ok)
            });
        }
        while let Some(Ok((id, pid, ok))) = set.join_next().await {
            if !ok {
                continue;
            }
            let key = backend_key(&id);
            if state.lifecycle.set_ready(&key, pid) {
                let detail = format!("(pid {pid})");
                lifecycle::mark(
                    &state.data_dir,
                    &key,
                    "ready",
                    Some(&detail),
                    Some(Origin::Monitor),
                );
            }
            if let Some(row) = rows.get_mut(&id) {
                row.state = LifecycleState::Running;
            }
        }
    }

    // 3. Diff and classify (not when the app began exiting while this
    //    tick ran: the rows read now reflect the deliberate release).
    if state.lifecycle.app_exiting() {
        return;
    }
    let scm = lifecycle::scm_snapshot(state, Duration::from_secs(1)).await;
    let mut ids: Vec<&String> = rows.keys().collect();
    ids.sort();
    for id in ids {
        let row = &rows[id];
        let key = backend_key(id);
        let rec = state.lifecycle.record(&key);
        let transition = rec.transition.is_some() || row.transition.is_some();
        let prev = state.lifecycle.observe(&key, row.state);
        if stopped_outside_app(row.service_managed, prev, row.state, transition)
            && rec.intent != Some(Intent::Stopped)
        {
            record_outside_stop(state, sink, &key);
            continue;
        }
        if started_outside_app(rec.intent, prev, row.state, transition) {
            record_outside_start(state, sink, &key, row.state, row.service_managed);
            continue;
        }
        match detect(prev, row.state, rec.intent, transition) {
            Some(Detected::Crashed) => {
                let (exit_status, exit_code, cause) = if row.service_managed {
                    let code = scm
                        .get(&manager_services::backend_service_name(id))
                        .and_then(lifecycle::scm_exit_code);
                    (
                        code.map(|c| format!("exit code {c}")),
                        code,
                        row.failure_reason.clone(),
                    )
                } else {
                    let reaped = reaped.iter().find(|(r, _, _)| r == id);
                    (
                        exits.get(id).cloned(),
                        reaped.and_then(|(_, _, c)| *c),
                        fatal.get(id).cloned(),
                    )
                };
                let reason = crash_reason(exit_status.as_deref(), cause.as_deref());
                record_crash(state, sink, &key, &reason, exit_code, marked.contains(&key));
                events.push(MonitorEvent::Crashed {
                    id: id.clone(),
                    exit_status,
                    reason: cause,
                });
            }
            Some(Detected::Recovered) => record_recovery(state, sink, &key, row.state),
            None => emit_change(sink, &key, prev, row.state, transition),
        }
    }
}

/// Step 4: the gateway.
async fn gateway(state: &AppState, sink: &dyn EventSink, events: &mut Vec<MonitorEvent>) {
    if state.lifecycle.transition(GATEWAY).is_some() {
        monitor_state(state).gateway_strikes = 0;
        return;
    }
    let generation = state.lifecycle.generation(GATEWAY);
    let view = lifecycle::gateway_view(state).await;
    if !result_is_current(
        generation,
        state.lifecycle.generation(GATEWAY),
        state.lifecycle.transition(GATEWAY).is_some(),
    ) {
        // The lifecycle moved on while the probe ran (GW-6).
        return;
    }
    let rec = state.lifecycle.record(GATEWAY);
    let obs = GatewayObs {
        state: Some(view.state),
        prev: rec.last_observed,
        up: view.up,
        pid: view.pid,
        intent: rec.intent,
        failure_recorded: rec.failure.is_some(),
        service_managed: view.service_managed,
        scm_failure: if rec.failure.is_none() {
            view.failure.as_ref().map(|f| f.reason.clone())
        } else {
            None
        },
    };
    let strikes = monitor_state(state).gateway_strikes;
    let action = gateway_action(&obs, strikes);
    let mut now = view.state;
    match &action {
        GatewayAction::SeedRunning => {
            state.lifecycle.set_intent(GATEWAY, Intent::Running);
            monitor_state(state).gateway_strikes = 0;
        }
        GatewayAction::Healthy => {
            monitor_state(state).gateway_strikes = 0;
            if rec.failure.is_some() {
                state.lifecycle.clear_failure(GATEWAY);
            }
        }
        GatewayAction::Strike => {
            monitor_state(state).gateway_strikes = strike(true, strikes).0;
        }
        GatewayAction::Down(reason) => {
            monitor_state(state).gateway_strikes = 0;
            let exit_code = view.failure.as_ref().and_then(|f| f.exit_code);
            state
                .lifecycle
                .set_failure(GATEWAY, Failure::now(reason.clone(), exit_code));
            lifecycle::mark(
                &state.data_dir,
                GATEWAY,
                "crashed",
                Some(reason),
                Some(Origin::Monitor),
            );
            lifecycle::emit(
                sink,
                GATEWAY,
                "crashed",
                LifecycleState::Failed,
                Some(Origin::Monitor),
                Some(reason),
            );
            now = if view.pid.is_some() || view.state == LifecycleState::Degraded {
                view.state
            } else {
                LifecycleState::Failed
            };
            state.lifecycle.observe(GATEWAY, now);
            events.push(MonitorEvent::GatewayDown {
                reason: reason.clone(),
            });
            return;
        }
        GatewayAction::StoppedOutside => {
            monitor_state(state).gateway_strikes = 0;
            state.lifecycle.observe(GATEWAY, now);
            record_outside_stop(state, sink, GATEWAY);
            return;
        }
        GatewayAction::StartedOutside => {
            monitor_state(state).gateway_strikes = 0;
            state.lifecycle.observe(GATEWAY, now);
            record_outside_start(state, sink, GATEWAY, now, view.service_managed);
            return;
        }
        GatewayAction::Nothing => {
            if view.pid.is_none() || view.state != LifecycleState::Degraded {
                monitor_state(state).gateway_strikes = 0;
            }
        }
    }
    let prev = state.lifecycle.observe(GATEWAY, now);
    if prev == Some(LifecycleState::Failed) && now == LifecycleState::Running {
        lifecycle::mark(
            &state.data_dir,
            GATEWAY,
            "recovered",
            Some("answering again"),
            Some(Origin::Monitor),
        );
    }
    emit_change(sink, GATEWAY, prev, now, false);
}

/// Step 5: PostgreSQL, while the gateway is expected up and neither
/// service has an operation in flight.
async fn postgres(state: &AppState, sink: &dyn EventSink, events: &mut Vec<MonitorEvent>) {
    let expected = state.lifecycle.intent(GATEWAY) == Some(Intent::Running);
    let busy = state.lifecycle.transition(GATEWAY).is_some()
        || state.lifecycle.transition(POSTGRES).is_some();
    if !expected || busy {
        monitor_state(state).postgres_strikes = 0;
        return;
    }
    let view = lifecycle::postgres_view(state).await;
    if view.transition.is_some() || state.lifecycle.transition(GATEWAY).is_some() {
        return;
    }
    let rec = state.lifecycle.record(POSTGRES);
    let failing = view.state == LifecycleState::Failed;
    let (strikes, fire) = {
        let mut m = monitor_state(state);
        let (n, fire) = strike(failing, m.postgres_strikes);
        m.postgres_strikes = n;
        (n, fire)
    };
    // While the first strike is pending the baseline keeps the previous
    // state, so the change is reported once, when it is confirmed.
    if failing && strikes < STRIKES {
        return;
    }
    let prev = state.lifecycle.observe(POSTGRES, view.state);
    if fire && rec.failure.is_none() {
        let reason = match view.pid {
            Some(pid) => format!("postmaster {pid} is not accepting connections"),
            None => "the server is not running".to_string(),
        };
        state
            .lifecycle
            .set_failure(POSTGRES, Failure::now(reason.clone(), None));
        lifecycle::mark(
            &state.data_dir,
            POSTGRES,
            "crashed",
            Some(&reason),
            Some(Origin::Monitor),
        );
        lifecycle::emit(
            sink,
            POSTGRES,
            "crashed",
            LifecycleState::Failed,
            Some(Origin::Monitor),
            Some(&reason),
        );
        events.push(MonitorEvent::PostgresDown { reason });
        return;
    }
    if view.state == LifecycleState::Running && rec.failure.is_some() {
        record_recovery(state, sink, POSTGRES, view.state);
        return;
    }
    emit_change(sink, POSTGRES, prev, view.state, false);
}

/// Step 6: the log receiver service.
async fn log_receiver(state: &AppState, sink: &dyn EventSink, events: &mut Vec<MonitorEvent>) {
    let view = lifecycle::log_receiver_view(state).await;
    let transition = view.transition.is_some();
    let prev = state.lifecycle.observe(LOG_RECEIVER, view.state);
    if !view.registered {
        return;
    }
    let rec = state.lifecycle.record(LOG_RECEIVER);
    if stopped_outside_app(true, prev, view.state, transition)
        && rec.intent != Some(Intent::Stopped)
    {
        record_outside_stop(state, sink, LOG_RECEIVER);
        return;
    }
    if started_outside_app(rec.intent, prev, view.state, transition) {
        record_outside_start(state, sink, LOG_RECEIVER, view.state, true);
        return;
    }
    match detect(prev, view.state, rec.intent, transition) {
        Some(Detected::Crashed) => {
            let reason = view
                .failure_reason
                .clone()
                .unwrap_or_else(|| "the service stopped".to_string());
            record_crash(state, sink, LOG_RECEIVER, &reason, view.exit_code, false);
            events.push(MonitorEvent::LogReceiverDown {
                exit_code: view.exit_code,
                reason: view.failure_reason,
            });
        }
        Some(Detected::Recovered) => record_recovery(state, sink, LOG_RECEIVER, view.state),
        None => emit_change(sink, LOG_RECEIVER, prev, view.state, transition),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_services::{ServiceState, ServiceStatus};
    use manager_supervisor::{ProcessState, ProcessStatus};

    use LifecycleState::*;

    #[test]
    fn operator_stop_is_not_a_crash() {
        assert_eq!(
            detect(Some(Running), Stopped, Some(Intent::Stopped), false),
            None
        );
        assert_eq!(
            detect(Some(Running), Failed, Some(Intent::Stopped), false),
            None
        );
    }

    #[test]
    fn unexpected_exit_with_intent_running_is_a_crash() {
        assert_eq!(
            detect(Some(Running), Stopped, Some(Intent::Running), false),
            Some(Detected::Crashed)
        );
        assert_eq!(
            detect(Some(Starting), Failed, Some(Intent::Running), false),
            Some(Detected::Crashed)
        );
        assert_eq!(
            detect(Some(Degraded), Failed, Some(Intent::Running), false),
            Some(Detected::Crashed)
        );
        // Without a recorded intent only a reported failure counts.
        assert_eq!(
            detect(Some(Running), Failed, None, false),
            Some(Detected::Crashed)
        );
        assert_eq!(detect(Some(Running), Stopped, None, false), None);
    }

    #[test]
    fn transitions_and_baselines_never_fire() {
        assert_eq!(
            detect(Some(Running), Failed, Some(Intent::Running), true),
            None
        );
        // The end of an operation that failed: the op reported it.
        assert_eq!(
            detect(Some(Restarting), Failed, Some(Intent::Running), false),
            None
        );
        assert_eq!(
            detect(Some(Stopping), Stopped, Some(Intent::Running), false),
            None
        );
        // First observation (or no change).
        assert_eq!(detect(None, Failed, Some(Intent::Running), false), None);
    }

    #[test]
    fn recovery_is_detected() {
        assert_eq!(
            detect(Some(Failed), Running, Some(Intent::Running), false),
            Some(Detected::Recovered)
        );
        assert_eq!(
            detect(Some(Failed), Running, Some(Intent::Running), true),
            None
        );
    }

    #[test]
    fn clean_scm_stop_is_an_outside_stop() {
        assert!(stopped_outside_app(true, Some(Running), Stopped, false));
        assert!(!stopped_outside_app(true, Some(Running), Failed, false));
        assert!(!stopped_outside_app(false, Some(Running), Stopped, false));
        assert!(!stopped_outside_app(true, Some(Running), Stopped, true));
        assert!(!stopped_outside_app(true, None, Stopped, false));
    }

    #[test]
    fn service_mode_stopped_with_code_1_is_a_crash_with_the_credential_reason() {
        // The shim reports ERROR_SERVICE_SPECIFIC_ERROR with code 1 when
        // the environment/credentials could not be resolved.
        let mut scm = HashMap::new();
        scm.insert(
            manager_services::backend_service_name("a"),
            ServiceStatus {
                name: manager_services::backend_service_name("a"),
                state: ServiceState::Stopped,
                pid: None,
                child_pid: None,
                win32_exit_code: manager_services::ERROR_SERVICE_SPECIFIC_ERROR,
                service_specific_exit_code: 1,
            },
        );
        let sup: Vec<ProcessStatus> = vec![ProcessStatus {
            id: "a".into(),
            state: ProcessState::Stopped,
            pid: None,
            adopted: false,
            restarts: 0,
            uptime: None,
            failure_reason: None,
        }];
        let rows = lifecycle::merge_backend_liveness(
            &sup,
            &scm,
            &HashMap::new(),
            &[("a".to_string(), 8081)],
            true,
            chrono::Utc::now(),
            &|_| None,
        );
        let row = &rows["a"];
        assert_eq!(row.state, Failed);
        assert!(row.service_managed);
        assert!(!stopped_outside_app(
            row.service_managed,
            Some(Running),
            row.state,
            false
        ));
        assert_eq!(
            detect(Some(Running), row.state, Some(Intent::Running), false),
            Some(Detected::Crashed)
        );
        let code = lifecycle::scm_exit_code(&scm[&manager_services::backend_service_name("a")]);
        assert_eq!(code, Some(1));
        let reason = crash_reason(
            code.map(|c| format!("exit code {c}")).as_deref(),
            row.failure_reason.as_deref(),
        );
        assert!(reason.contains("environment/credentials"), "{reason}");
        assert!(
            reason.starts_with("exited unexpectedly (exit code 1)"),
            "{reason}"
        );
    }

    #[test]
    fn crash_reason_shapes() {
        assert_eq!(
            crash_reason(Some("exit code: 3"), None),
            "exited unexpectedly (exit code: 3)"
        );
        assert_eq!(
            crash_reason(Some("exit code: 1"), Some("GPU unavailable")),
            "exited unexpectedly (exit code: 1). GPU unavailable"
        );
        assert_eq!(crash_reason(None, Some("x")), "x");
        assert_eq!(crash_reason(None, None), "is no longer running");
    }

    #[test]
    fn observed_event_names() {
        assert_eq!(observed_event(Some(Starting), Running), "ready");
        assert_eq!(observed_event(Some(Stopped), Running), "started");
        assert_eq!(observed_event(Some(Running), Degraded), "degraded");
        assert_eq!(observed_event(Some(Running), Stopped), "stopped");
    }

    fn gw(up: bool, pid: Option<u32>, state: LifecycleState, intent: Option<Intent>) -> GatewayObs {
        GatewayObs {
            state: Some(state),
            up,
            pid,
            intent,
            ..Default::default()
        }
    }

    #[test]
    fn gateway_actions() {
        // Up at launch with no intent: now expected up.
        assert_eq!(
            gateway_action(&gw(true, Some(5), Running, None), 0),
            GatewayAction::SeedRunning
        );
        assert_eq!(
            gateway_action(&gw(true, Some(5), Running, Some(Intent::Running)), 1),
            GatewayAction::Healthy
        );
        // Down but never expected up, or stopped by the operator.
        assert_eq!(
            gateway_action(&gw(false, None, Stopped, None), 0),
            GatewayAction::Nothing
        );
        assert_eq!(
            gateway_action(&gw(false, None, Stopped, Some(Intent::Stopped)), 0),
            GatewayAction::Nothing
        );
        // The process is gone: down at once.
        assert_eq!(
            gateway_action(&gw(false, None, Stopped, Some(Intent::Running)), 0),
            GatewayAction::Down("process exited".into())
        );
        // Alive but not answering: two strikes past the start grace.
        let hung = gw(false, Some(7), Degraded, Some(Intent::Running));
        assert_eq!(gateway_action(&hung, 0), GatewayAction::Strike);
        assert_eq!(
            gateway_action(&hung, 1),
            GatewayAction::Down("process 7 is not answering".into())
        );
        // Still within the start grace.
        assert_eq!(
            gateway_action(&gw(false, Some(7), Starting, Some(Intent::Running)), 5),
            GatewayAction::Nothing
        );
        // Reported once: a recorded failure latches it.
        let mut latched = gw(false, None, Failed, Some(Intent::Running));
        latched.failure_recorded = true;
        assert_eq!(gateway_action(&latched, 0), GatewayAction::Nothing);
        // Service route: a clean SCM stop is an outside stop; an SCM
        // failure carries its reason.
        let mut svc = gw(false, None, Stopped, Some(Intent::Running));
        svc.service_managed = true;
        assert_eq!(gateway_action(&svc, 0), GatewayAction::StoppedOutside);
        svc.state = Some(Failed);
        svc.scm_failure = Some("child exited with code 3".into());
        assert_eq!(
            gateway_action(&svc, 0),
            GatewayAction::Down("child exited with code 3".into())
        );
        // Service host running, child pid not resolved yet: alive, so a
        // strike past the grace, never "process exited".
        let mut host = gw(false, None, Degraded, Some(Intent::Running));
        host.service_managed = true;
        assert_eq!(gateway_action(&host, 0), GatewayAction::Strike);
        host.state = Some(Starting);
        assert_eq!(gateway_action(&host, 0), GatewayAction::Nothing);
    }

    #[test]
    fn outside_stop_then_outside_start_then_crash_is_reported() {
        // Gateway: stopped outside the app latches intent Stopped ...
        let mut svc = gw(false, None, Stopped, Some(Intent::Running));
        svc.service_managed = true;
        svc.prev = Some(Running);
        assert_eq!(gateway_action(&svc, 0), GatewayAction::StoppedOutside);
        // ... started outside the app re-arms it ...
        let mut back = gw(true, Some(9), Running, Some(Intent::Stopped));
        back.service_managed = true;
        back.prev = Some(Stopped);
        assert_eq!(gateway_action(&back, 0), GatewayAction::StartedOutside);
        // ... so the next crash is reported.
        let mut gone = gw(false, None, Stopped, Some(Intent::Running));
        gone.prev = Some(Running);
        assert_eq!(
            gateway_action(&gone, 0),
            GatewayAction::Down("process exited".into())
        );
        // An operator stop that stays stopped is not a start.
        let mut still = gw(false, None, Stopped, Some(Intent::Stopped));
        still.prev = Some(Stopped);
        assert_eq!(gateway_action(&still, 0), GatewayAction::Nothing);

        // Backends / log receiver: the same sequence through the pure rules.
        assert!(stopped_outside_app(true, Some(Running), Stopped, false));
        assert!(started_outside_app(
            Some(Intent::Stopped),
            Some(Stopped),
            Running,
            false
        ));
        assert!(started_outside_app(
            Some(Intent::Stopped),
            Some(Stopped),
            Starting,
            false
        ));
        assert!(!started_outside_app(
            Some(Intent::Stopped),
            Some(Stopped),
            Running,
            true
        ));
        assert!(!started_outside_app(
            Some(Intent::Running),
            Some(Stopped),
            Running,
            false
        ));
        assert!(!started_outside_app(
            Some(Intent::Stopped),
            None,
            Running,
            false
        ));
        // After the re-arm (intent Running) the crash is detected.
        assert_eq!(
            detect(Some(Running), Stopped, Some(Intent::Running), false),
            Some(Detected::Crashed)
        );
    }

    #[test]
    fn strikes_count_and_reset() {
        assert_eq!(strike(true, 0), (1, false));
        assert_eq!(strike(true, 1), (2, true));
        assert_eq!(strike(true, 2), (3, false), "fires once");
        assert_eq!(strike(false, 2), (0, false));
    }

    #[test]
    fn stale_generation_discards_the_gateway_result() {
        // The operator's stop bumps the generation while a probe runs: the
        // result taken under the old generation is discarded.
        let reg = std::sync::Arc::new(lifecycle::LifecycleRegistry::new());
        let before = reg.generation(GATEWAY);
        assert!(result_is_current(before, reg.generation(GATEWAY), false));
        reg.set_intent(GATEWAY, Intent::Stopped);
        assert!(!result_is_current(before, reg.generation(GATEWAY), false));
        // A transition that began meanwhile also makes it stale.
        let g = reg
            .try_begin(GATEWAY, lifecycle::TransitionKind::Restarting, Origin::Ui)
            .unwrap();
        let during = reg.generation(GATEWAY);
        assert!(!result_is_current(during, reg.generation(GATEWAY), true));
        drop(g);
        // Under the new intent the same "down" is an operator stop.
        assert_eq!(
            gateway_action(&gw(false, None, Stopped, reg.intent(GATEWAY)), 0),
            GatewayAction::Nothing
        );
    }
}
