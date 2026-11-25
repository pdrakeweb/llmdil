//! Setup wizard, audit, and the additional setup (§14) operations.

use crate::*;

/// What one wizard step run produced ([`run_wizard_step_inner`]).
pub(crate) struct WizardRun {
    pub task_id: String,
    /// The refusal payload when the run did not happen: another setup
    /// step holds the wizard, or another gateway operation is in
    /// flight (`busy: true`, `ok: false`, `running_step`,
    /// `elapsed_secs`).
    pub busy: Option<Value>,
    pub outcome: Option<StepOutcome>,
    /// Whether an activation plan was pending before the step ran
    /// (start-gateway only; CR-1b).
    pub pending_before: bool,
}

impl WizardRun {
    /// The step ran and ended Done (or Skipped).
    pub fn ok(&self) -> bool {
        self.busy.is_none()
            && self
                .outcome
                .as_ref()
                .is_some_and(|o| matches!(o.state, StepState::Done | StepState::Skipped))
    }

    /// The op payload: the busy payload as is, else
    /// `{task_id, ok, outcome}`.
    pub(crate) fn payload(&self) -> Value {
        if let Some(busy) = &self.busy {
            return busy.clone();
        }
        json!({ "task_id": self.task_id, "ok": self.ok(), "outcome": self.outcome })
    }
}

/// The activation plan as a gateway start found it, for the
/// completion bookkeeping after the start ([`complete_gateway_activation`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct GatewayPlanBefore {
    /// Any unresolved entry, or drift.
    pub pending: bool,
    pub drift: bool,
    /// The unresolved gateway-scope entries.
    pub gateway_entries: Vec<activation::ActivationEntry>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl GatewayPlanBefore {
    pub(crate) async fn capture(state: &AppState) -> Self {
        let (unresolved, drift) = activation_unresolved(state).await;
        Self {
            pending: !unresolved.is_empty() || drift,
            drift,
            gateway_entries: unresolved
                .into_iter()
                .filter(|e| e.scope != "backend")
                .collect(),
            started_at: Some(chrono::Utc::now()),
        }
    }
}

/// Run one wizard step; streams `wizard-progress`. The payload is
/// `{task_id, ok, outcome}` (`ok`: the step ended Done or Skipped), or
/// the busy payload. `origin` names the surface that asked (default
/// `wizard`); for start-gateway it lands in the gateway's lifecycle
/// markers and the activation history.
pub async fn op_run_wizard_step(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    step_id: String,
    origin: Option<String>,
) -> ApiResult {
    let origin = origin
        .as_deref()
        .map(lifecycle::Origin::parse)
        .unwrap_or(lifecycle::Origin::Wizard);
    let run = run_wizard_step_inner(state, sink, &step_id, origin, None, None).await?;
    Ok(run.payload())
}

/// [`op_run_wizard_step`] with an optional parent cancellation token:
/// cancelling `parent` stops the step exactly like a `task_abort` of
/// the step's own task. Component updates (`updates` module) run the
/// wizard's install steps through this, so aborting the update task
/// reaches the step it is running. Origin `wizard`.
pub(crate) async fn run_wizard_step_linked(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    step_id: String,
    parent: Option<tokio_util::sync::CancellationToken>,
) -> ApiResult {
    let run = run_wizard_step_inner(
        state,
        sink,
        &step_id,
        lifecycle::Origin::Wizard,
        None,
        parent,
    )
    .await?;
    Ok(run.payload())
}

/// The CR-5 guard of the install-llamacpp step: the step extracts the
/// llama.cpp build over `<data_dir>/llamacpp` in place, which fails on
/// (or partially overwrites) binaries a running llama-server has
/// mapped. Refused while any backend is live or mid-transition.
pub(crate) async fn refuse_llamacpp_update_while_running(state: &AppState) -> Result<(), ApiError> {
    let live = lifecycle::backend_liveness(state).await;
    let mut busy: Vec<String> = live
        .values()
        .filter(|b| b.state.is_live() || b.transition.is_some())
        .map(|b| b.id.clone())
        .collect();
    if busy.is_empty() {
        return Ok(());
    }
    busy.sort();
    Err(ApiError::InvalidInput(format!(
        "stop backends {} before updating llama.cpp",
        busy.join(", ")
    )))
}

/// The busy payload of a refused step run.
pub(crate) fn wizard_busy_payload(task_id: &str, running: Option<(String, u64)>) -> Value {
    let mut payload = json!({ "task_id": task_id, "busy": true, "ok": false, "outcome": null });
    if let Some((id, secs)) = running {
        payload["running_step"] = json!(id);
        payload["elapsed_secs"] = json!(secs);
    }
    payload
}

/// One wizard step run, shared by `run_wizard_step` and
/// `gateway_restart`.
///
/// start-gateway runs under the gateway's lifecycle transition: the
/// caller's `gateway_guard` (gateway_restart), else one taken here —
/// `restarting` when a gateway is running or its process is alive,
/// else `starting`. Another gateway operation in flight refuses the
/// run with the busy payload (`running_step: "gateway <kind>
/// (<origin>)"`), before anything is touched. A Done outcome sets
/// intent Running, completes the gateway side of the activation plan
/// ([`complete_gateway_activation`]) and finishes the transition
/// `started`/`restarted`; any other outcome finishes it failed with
/// the step's message. `parent` (component updates) cancels the step
/// like a `task_abort` of its own task.
pub(crate) async fn run_wizard_step_inner(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    step_id: &str,
    origin: lifecycle::Origin,
    gateway_guard: Option<lifecycle::TransitionGuard>,
    parent: Option<tokio_util::sync::CancellationToken>,
) -> Result<WizardRun, ApiError> {
    let is_gateway = step_id == "start-gateway";
    if step_id == "install-llamacpp" {
        refuse_llamacpp_update_while_running(state).await?;
    }
    let task_id = state
        .record_task(
            "wizard_step",
            json!({ "step_id": step_id, "origin": origin }),
        )
        .await;
    let mut gateway_guard = gateway_guard;
    if is_gateway && gateway_guard.is_none() {
        let view = lifecycle::gateway_view(state).await;
        let kind = if matches!(
            view.state,
            lifecycle::LifecycleState::Running | lifecycle::LifecycleState::Degraded
        ) || view.pid.is_some()
        {
            lifecycle::TransitionKind::Restarting
        } else {
            lifecycle::TransitionKind::Starting
        };
        match lifecycle::begin(state, sink, lifecycle::GATEWAY, kind, origin, None).await {
            Ok(g) => gateway_guard = Some(g),
            Err(ApiError::Busy(msg)) => {
                let running = state.lifecycle.transition(lifecycle::GATEWAY).map(|t| {
                    (
                        format!("gateway {} ({})", t.kind, t.origin),
                        (chrono::Utc::now() - t.since).num_seconds().max(0) as u64,
                    )
                });
                append_wizard_log(
                    &state.data_dir,
                    &[format!(
                        "[{}] === step {step_id} (install): rejected — {msg} ===",
                        chrono::Utc::now().to_rfc3339(),
                    )],
                );
                let payload = wizard_busy_payload(&task_id, running);
                let mut detail = payload.clone();
                detail["message"] = json!(msg);
                state.finish_task(&task_id, TaskStatus::Done, detail).await;
                let mut payload = payload;
                payload["message"] = json!(msg);
                return Ok(WizardRun {
                    task_id,
                    busy: Some(payload),
                    outcome: None,
                    pending_before: false,
                });
            }
            Err(e) => return Err(e),
        }
    }
    let ctx = state.wizard_ctx(RunMode::Install).await;
    let wizard = state.wizard(RunMode::Install).await;
    // try_lock, not lock: a second invocation (e.g. a stray double
    // click while a step runs) must not queue on the mutex and execute
    // a step again after the current run finishes. Like the audit, it
    // reports the running state instead of failing. The report names
    // the in-flight step and how long it has been running, and the
    // rejection is written to the wizard log: a bare "busy" with no
    // record was undiagnosable when a step wedged the lock
    // (2026-10-16).
    let mut guard = match wizard.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            let running = state
                .wizard
                .step_running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|(id, since)| (id.clone(), since.elapsed().as_secs()));
            let reason = match &running {
                Some((id, secs)) => format!("step {id} is already running (started {secs}s ago)"),
                None => "another setup step is already running".to_string(),
            };
            append_wizard_log(
                &state.data_dir,
                &[format!(
                    "[{}] === step {step_id} (install): rejected — {reason} ===",
                    chrono::Utc::now().to_rfc3339(),
                )],
            );
            // The gateway transition taken for this run ends without
            // touching the gateway: nothing failed, so no failure is
            // recorded — only the refusal is marked.
            if let Some(g) = gateway_guard.take() {
                lifecycle::mark(
                    &state.data_dir,
                    lifecycle::GATEWAY,
                    &format!("{} refused", g.kind().verb()),
                    Some(&reason),
                    Some(origin),
                );
                drop(g);
                // The begin announced a transition: settle every
                // event-driven surface back on the measured state.
                let settled = lifecycle::gateway_view(state).await.state;
                state.lifecycle.observe(lifecycle::GATEWAY, settled);
                lifecycle::emit(
                    sink.as_ref(),
                    lifecycle::GATEWAY,
                    "refused",
                    settled,
                    Some(origin),
                    Some(&reason),
                );
            }
            let payload = wizard_busy_payload(&task_id, running);
            let mut detail = json!({ "busy": true });
            if let Some(step) = payload.get("running_step") {
                detail["running_step"] = step.clone();
                detail["elapsed_secs"] = payload["elapsed_secs"].clone();
            }
            state.finish_task(&task_id, TaskStatus::Done, detail).await;
            return Ok(WizardRun {
                task_id,
                busy: Some(payload),
                outcome: None,
                pending_before: false,
            });
        }
    };
    // Captured BEFORE the step runs: start-gateway rewrites the
    // gateway configuration (and, on success, the applied stamp), so
    // the plan read afterwards no longer says whether this run was
    // applying it (CR-1b).
    let plan_before = if is_gateway {
        state
            .lifecycle
            .set_intent(lifecycle::GATEWAY, lifecycle::Intent::Running);
        GatewayPlanBefore::capture(state).await
    } else {
        GatewayPlanBefore::default()
    };
    let (_done_before, total) = guard.progress();
    // A start marker lands in the wizard log before the step runs:
    // a step that hangs (the install-postgres case) then leaves a
    // timestamped "started, never finished" record instead of nothing.
    append_wizard_log(
        &state.data_dir,
        &[format!(
            "[{}] === step {step_id} (install): started ===",
            chrono::Utc::now().to_rfc3339()
        )],
    );
    // Track the in-flight step for the busy arm above; cleared on
    // every path out of the run, including a step error.
    *state
        .wizard
        .step_running
        .lock()
        .unwrap_or_else(|e| e.into_inner()) =
        Some((step_id.to_string(), std::time::Instant::now()));
    // task_abort cancels the token: the runner stops the step (its
    // command's process tree is killed) and records it failed.
    let cancel_token = state.tasks.controls.token(&task_id);
    let parent = parent.unwrap_or_default();
    let cancelled = async {
        tokio::select! {
            _ = cancel_token.cancelled() => {}
            _ = parent.cancelled() => {}
        }
    };
    let outcome_result = guard
        .run_step_until(&ctx, step_id, std::pin::pin!(cancelled))
        .await;
    *state
        .wizard
        .step_running
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    let (mut outcome, step_cancelled): (StepOutcome, bool) = match outcome_result {
        Ok(r) => r,
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": e.to_string() }),
                )
                .await;
            if let Some(g) = gateway_guard.take() {
                lifecycle::finish(state, sink.as_ref(), g, Err(e.to_string()));
            }
            return Err(ApiError::from(e));
        }
    };
    // The step returned its config edits instead of saving them;
    // apply them under the config lock (PS-4). A failed save fails
    // the step.
    for (id, message) in state.apply_wizard_config_edits(&mut guard).await {
        if id == step_id {
            outcome.state = StepState::Failed;
            outcome.log_tail.push(message.clone());
            outcome.message = message;
        }
    }
    append_wizard_log(
        &state.data_dir,
        &wizard_log_block(
            "install",
            step_id,
            &format!("{:?}", outcome.state).to_lowercase(),
            &outcome.message,
            outcome.duration_ms,
            &outcome.log_tail,
        ),
    );
    let (done, _) = guard.progress();
    *state.wizard.steps_cache.write().await = Some(wizard_steps_payload(&guard));
    // The step's display name, for the notification below (the
    // report is dropped with the guard).
    let step_name = guard
        .report()
        .steps
        .iter()
        .find(|s| s.id == step_id)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| step_id.to_string());
    drop(guard);
    state.persist_wizard().await;
    state.reload_config_from_disk().await;
    // A step run can complete the checklist (this is also the path
    // the wizard's "Run setup" full run takes, once per step).
    state.reconcile_setup_completed().await;
    // The gateway's own lifecycle record: intent, failure, the
    // activation completion and the started/restarted marker.
    if let Some(g) = gateway_guard.take() {
        if outcome.state == StepState::Done {
            state.lifecycle.clear_failure(lifecycle::GATEWAY);
            rearm_postgres_after_gateway_start(state);
            complete_gateway_activation(state, sink, origin, &plan_before).await;
            let pid = gateway_pid_now(state).await;
            lifecycle::finish(
                state,
                sink.as_ref(),
                g,
                Ok(pid.map(|p| format!("pid {p}")).unwrap_or_default()),
            );
        } else {
            let reason = outcome
                .message
                .lines()
                .next()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!("start-gateway ended {:?}", outcome.state).to_lowercase()
                });
            lifecycle::finish(state, sink.as_ref(), g, Err(reason));
        }
    }
    let payload = WizardProgressPayload {
        task_id: task_id.clone(),
        step_id: Some(step_id.to_string()),
        state: outcome.state,
        done,
        total,
        message: outcome.message.clone(),
    };
    sink.emit(
        events::WIZARD_PROGRESS,
        serde_json::to_value(&payload).unwrap_or(Value::Null),
    );
    state
        .finish_task(
            &task_id,
            if step_cancelled {
                TaskStatus::Aborted
            } else {
                TaskStatus::Done
            },
            serde_json::to_value(&outcome).unwrap_or(Value::Null),
        )
        .await;
    // Toasts: a step run is a discrete, operator-visible event (one
    // toast per run, at its terminal state). A failed start-gateway
    // while an activation plan was pending is the plan failing — one
    // toast, the Activation one, not both categories.
    let step_body = if outcome.message.trim().is_empty() {
        step_name.clone()
    } else {
        format!("{step_name}: {}", outcome.message)
    };
    // An operator-aborted step stays silent, like a cancelled
    // download: the operator already knows.
    let toast_state = (!step_cancelled).then_some(outcome.state);
    match toast_state {
        Some(StepState::Done) => {
            notify::notify(
                sink.as_ref(),
                state,
                notify::Category::Operations,
                "Setup step finished",
                &step_body,
            )
            .await;
        }
        Some(StepState::Failed) => {
            if is_gateway && plan_before.pending {
                notify::notify(
                    sink.as_ref(),
                    state,
                    notify::Category::Activation,
                    "Activation plan failed",
                    &format!(
                        "The gateway could not be started to apply the saved changes: {}",
                        outcome.message
                    ),
                )
                .await;
            } else {
                notify::notify(
                    sink.as_ref(),
                    state,
                    notify::Category::Failures,
                    "Setup step failed",
                    &step_body,
                )
                .await;
            }
        }
        Some(StepState::Manual) => {
            notify::notify(
                sink.as_ref(),
                state,
                notify::Category::Failures,
                "Setup step needs manual action",
                &step_body,
            )
            .await;
        }
        _ => {}
    }
    Ok(WizardRun {
        task_id,
        busy: None,
        outcome: Some(outcome),
        pending_before: plan_before.pending,
    })
}

/// The live gateway pid (pid file, else the gateway port's owner).
pub(crate) async fn gateway_pid_now(state: &AppState) -> Option<u32> {
    let port = state.config.read().await.gateway.port;
    let data_dir = state.data_dir.clone();
    tokio::task::spawn_blocking(move || manager_wizard::resolve_gateway_pid(&data_dir, port))
        .await
        .ok()
        .flatten()
        .map(|(pid, _)| pid)
}

/// Re-run audit: re-executes check-type steps without reinstalling (spec §4.1).
pub async fn op_run_audit(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    let task_id = state.record_task("audit", json!({})).await;
    let ctx = state.wizard_ctx(RunMode::Audit).await;
    let wizard = state.wizard(RunMode::Audit).await;
    // try_lock, not lock: a duplicate audit invocation must not queue
    // a second execution. But a concurrent audit (the launch-time
    // hydration audit, or a second click from a re-opened view) is a
    // state to report, not a failure — the running audit's results
    // land in the checklist when it finishes.
    let mut guard = match wizard.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Done,
                    json!({ "already_running": true }),
                )
                .await;
            return Ok(json!({
                "task_id": task_id,
                "already_running": true,
                "done": 0,
                "total": 0,
                "steps": [],
            }));
        }
    };
    // task_abort stops the running check and starts no further one.
    let cancel_token = state.tasks.controls.token(&task_id);
    let _ = guard
        .run_all_until(&ctx, std::pin::pin!(cancel_token.cancelled()))
        .await;
    let audit_cancelled = cancel_token.is_cancelled();
    // Audit-verified models return verified_at stamps (PS-4).
    state.apply_wizard_config_edits(&mut guard).await;
    let report: WizardReport = guard.report();
    for s in &report.steps {
        append_wizard_log(
            &state.data_dir,
            &wizard_log_block(
                "audit",
                &s.id,
                &format!("{:?}", s.state).to_lowercase(),
                &s.message,
                s.duration_ms,
                &s.log_tail,
            ),
        );
    }
    let (done, total) = report.progress();
    // The per-step results ride along in the response: the checklist is
    // painted from the install wizard's state (setup_get_steps), which
    // an audit never changes, so without this payload the audit's
    // outcome is invisible in the UI.
    let steps: Vec<Value> = report
        .steps
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "state": s.state,
                "message": s.message,
                "duration_ms": s.duration_ms,
                "log_tail": s.log_tail,
                "note": s.note,
                "manual_command": s.manual_command,
                "remediation": guard.remediation_hint(&s.id),
                "help_url": guard.help_url(&s.id),
            })
        })
        .collect();
    drop(guard);
    state.reload_config_from_disk().await;
    let payload = WizardProgressPayload {
        task_id: task_id.clone(),
        step_id: None,
        state: StepState::Done,
        done,
        total,
        message: format!("audit complete: {done}/{total} checks passed"),
    };
    sink.emit(
        events::WIZARD_PROGRESS,
        serde_json::to_value(&payload).unwrap_or(Value::Null),
    );
    state
        .finish_task(
            &task_id,
            if audit_cancelled {
                TaskStatus::Aborted
            } else {
                TaskStatus::Done
            },
            json!({ "done": done, "total": total }),
        )
        .await;
    Ok(json!({ "task_id": task_id, "done": done, "total": total, "steps": steps }))
}

/// Prerequisite verification matrix: per-step prerequisite lists from
/// [`manager_wizard::steps::default_steps`] (each step's `prerequisites()`
/// are the step ids that must be Done or Manual before it may run).
pub async fn op_setup_get_prerequisites(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let _ = state;
    let steps: Vec<Value> = manager_wizard::steps::default_steps()
        .iter()
        .map(|s| {
            json!({
                "id": s.id(),
                "name": s.name(),
                "prerequisites": s.prerequisites(),
            })
        })
        .collect();
    Ok(json!({ "steps": steps }))
}

/// Reset a failed step to Queued so it can run again.
pub async fn op_setup_retry_step(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.retry(&step_id);
    state.persist_wizard().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}

/// Skip a step with a note.
pub async fn op_setup_skip_step(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
    note: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.skip(&step_id, note);
    state.persist_wizard().await;
    state.reconcile_setup_completed().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}

/// Exact commands a step will run ("Show commands" UI). Must equal the argv
/// actually spawned by the step.
pub async fn op_setup_get_commands(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let ctx = state.wizard_ctx(RunMode::Install).await;
    let wizard = state.wizard(RunMode::Install).await;
    let guard = wizard.lock().await;
    let step = guard
        .get_step(&step_id)
        .ok_or_else(|| ApiError::NotFound(format!("wizard step {step_id}")))?;
    let commands = step.planned_commands(&ctx);
    Ok(serde_json::to_value(&commands)?)
}

/// The setup steps payload shared by `setup_get_steps` and the
/// last-known snapshot cached in AppState.
pub(crate) fn wizard_steps_payload(guard: &Wizard) -> Value {
    let report = guard.report();
    let steps: Vec<Value> = report
        .steps
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "state": s.state,
                "message": s.message,
                "duration_ms": s.duration_ms,
                "log_tail": s.log_tail,
                "note": s.note,
                "manual_command": s.manual_command,
                "remediation": guard.remediation_hint(&s.id),
                "help_url": guard.help_url(&s.id),
            })
        })
        .collect();
    let (done, total) = report.progress();
    json!({ "steps": steps, "done": done, "total": total })
}

/// Live wizard step states for the Setup view: id, name, state, last
/// outcome message/log tail, plus remediation + help URL per step.
/// While a step is executing (the wizard lock is held for the whole
/// run), the last known snapshot is served with `busy: true` instead
/// of blocking — blocking left the checklist blank for the duration
/// of long steps.
pub async fn op_setup_get_steps(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    match wizard.try_lock() {
        Ok(guard) => {
            let payload = wizard_steps_payload(&guard);
            *state.wizard.steps_cache.write().await = Some(payload.clone());
            drop(guard);
            // Reading the steps is also a healing trigger for the
            // launch-routing flag (the wizard view polls this while
            // the user works through setup in any order).
            state.reconcile_setup_completed().await;
            Ok(payload)
        }
        Err(_) => {
            // A step holds the wizard lock for its entire execution;
            // blocking on it here left the checklist blank for the
            // duration of the step. Serve the last known snapshot,
            // flagged busy, instead.
            match state.wizard.steps_cache.read().await.clone() {
                Some(mut payload) => {
                    payload["busy"] = json!(true);
                    Ok(payload)
                }
                None => Ok(json!({ "steps": [], "done": 0, "total": 0, "busy": true })),
            }
        }
    }
}

/// Whether the user has finished the setup wizard. The UI opens the wizard
/// on launch until this is true, so a complete checklist sets the flag
/// here first (`AppState::reconcile_setup_completed`).
pub async fn op_setup_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    state.reconcile_setup_completed().await;
    let cfg = state.config.read().await;
    Ok(json!({ "setup_completed": cfg.setup_completed }))
}

/// Mark the setup wizard complete and persist the flag: the flag is
/// set on a clone of the config, the clone is saved via
/// `manager_config::save`, and only a successful save installs it in
/// memory — a failed save returns an error rather than an ok:true
/// payload, so the UI never reports completion that a restart would
/// silently undo.
pub async fn op_setup_mark_complete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.setup_completed = true;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Help URL a wizard step offers alongside its remediation hint
/// (e.g. the NVIDIA driver download page on the hardware-detect step).
/// Returns `{"url": string|null, "step_id": string}`.
pub async fn op_setup_step_help_url(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    let url = wizard.lock().await.help_url(&step_id).map(String::from);
    Ok(json!({ "url": url, "step_id": step_id }))
}

/// Open an external URL in the system browser. Only http/https URLs are
/// accepted; anything else is rejected before spawning.
pub async fn op_open_url(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    url: String,
) -> ApiResult {
    let ok = url.starts_with("http://") || url.starts_with("https://");
    if !ok {
        return Err(ApiError::InvalidInput(
            "open_url accepts http/https URLs only".to_string(),
        ));
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", &url]);
        // CREATE_NO_WINDOW suppresses only the transient cmd console;
        // the browser that `start` opens is a separate GUI process and
        // still appears.
        cmd.creation_flags(0x08000000);
        cmd.spawn()
            .map_err(|e| ApiError::Io(format!("failed to open URL: {e}")))?;
        Ok(json!({ "ok": true }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(ApiError::Unsupported(
            "open_url is only supported on Windows".to_string(),
        ))
    }
}

/// The gateway master key, for the Gateway view's client-endpoint
/// card: it is both the API key local clients present and the
/// LiteLLM admin UI password (username `admin`). Read from Windows
/// Credential Manager, generating and storing on first use — the
/// same ensure the gateway spawn and the editor-config writer use,
/// so the value shown is the value in force.
pub async fn op_gateway_master_key(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let key = manager_wizard::ensure_gateway_master_key(&RealSystemOps::default()).await?;
    Ok(json!({ "master_key": key }))
}

/// Open a folder in Windows Explorer. `path` may name the folder
/// itself or a file inside it (the diagnostics bundle path is a
/// file; its containing folder is opened). Backs the "Open folder"
/// button shown after a diagnostics export.
pub async fn op_open_folder(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    path: String,
) -> ApiResult {
    let p = std::path::Path::new(&path);
    let dir = if p.is_dir() {
        p.to_path_buf()
    } else if p.is_file() {
        match p.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                return Err(ApiError::InvalidInput(format!(
                    "no containing folder for: {path}"
                )))
            }
        }
    } else {
        return Err(ApiError::InvalidInput(format!(
            "path does not exist: {path}"
        )));
    };
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("explorer.exe");
        cmd.arg(&dir);
        // CREATE_NO_WINDOW: no console host for the spawn; Explorer
        // itself is a GUI process and still appears.
        cmd.creation_flags(0x08000000);
        cmd.spawn()
            .map_err(|e| ApiError::Io(format!("failed to open folder: {e}")))?;
        Ok(json!({ "ok": true }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = dir;
        Err(ApiError::Unsupported(
            "open_folder is only supported on Windows".to_string(),
        ))
    }
}

/// Mark a step as manually completed by the user.
pub async fn op_setup_mark_manual(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
    path: String,
    version: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.mark_manual(&step_id, path, version);
    state.persist_wizard().await;
    state.reconcile_setup_completed().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}

#[cfg(test)]
mod tests;
