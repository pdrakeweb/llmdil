//! Gateway activation (plan / cancel / complete / history), restart,
//! stop, quiesce, and the Postgres companion lifecycle.

use crate::*;

/// The activation plan: every saved change that is not live yet,
/// across scopes — gateway changes waiting on a gateway restart and
/// backend changes (flags, tensor split, model assignment) waiting
/// on their backend's (re)start — with the state the operator's
/// decision depends on. Each change carries its `activation`:
/// `gateway_restart`, `restart` (a running backend must restart),
/// or `next_start` (nothing running holds the old state; the change
/// takes effect when the relevant process next starts). Changes
/// whose targets are all stopped — or whose gateway is down — are
/// already effective by that same rule and are NOT part of the
/// plan: the plan exists only while a running process holds
/// pre-change state (activation::pending_entries). The payload's
/// `drift` follows that rule too: it reports live drift against
/// the RUNNING gateway only — while the gateway is down a stale
/// generated file is not pending work (it regenerates at the next
/// start) and is never stated as the plan's reason
/// (activation::drift_is_pending). When drift is the pending
/// work it also appears in `changes` as a synthesized
/// `gateway_drift` item (activation::drift_plan_change), so a
/// pending plan's change count is never zero (tracker #57); the
/// raw comparison rides along as `files_differ`, informational
/// in any gateway state.
pub async fn op_gateway_activation_plan(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let journal = activation::load_journal(&state.data_dir);
    let started_at = backend_start_times(state, &cfg).await;
    let running = backend_running_ids(state, &cfg).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let (view, _) = running_gateway_view(state).await;
    // `gateway_up` stays the probe; the plan's pending rule treats a
    // gateway with an operation in flight as up, so a restart's down
    // window never makes the plan vanish (GW-10 / ACT-5).
    let applying = activation_apply::current_progress(state);
    let probe_up = view.up;
    let up = probe_up || view.transition.is_some() || applying.is_some();
    let pending_entries = activation::pending_entries(
        &state.data_dir,
        &journal,
        &started_at,
        &configured,
        &running,
        up,
    );
    let presence = mcp_credential_presence(&[&cfg]).await;
    // Drift enters the plan only as pending work: while the
    // gateway is down a stale generated file regenerates at its
    // next start, so the raw file comparison must not surface in
    // the plan — a backend-only plan otherwise asserts gateway
    // drift its apply never touches (tracker #45; the rule lives
    // in activation::drift_is_pending). The raw comparison rides
    // the payload separately as `files_differ`, informational
    // only, so the plan view can note a stale file the next start
    // will regenerate without calling it pending.
    let drift_detail =
        activation::config_drift_detail(&cfg, &state.data_dir, |t| presence.contains(t));
    let drift = activation::drift_unexplained(
        activation::drift_is_pending(drift_detail.any(), up),
        &pending_entries,
    );
    let pending = !pending_entries.is_empty() || drift;
    let entry_ts = |e: &activation::ActivationEntry| {
        chrono::DateTime::parse_from_rfc3339(&e.ts)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    // Needs a restart when a target is running a process started
    // before the change (or with unknown start — an adopted process:
    // restarting is the safe call).
    let stale = |e: &activation::ActivationEntry, t: &String| {
        let ts = entry_ts(e);
        running.contains(t)
            && started_at
                .get(t)
                .map(|s| ts.map(|ts| *s < ts).unwrap_or(true))
                .unwrap_or(true)
    };
    let mut backend_restart_ids: Vec<String> = Vec::new();
    for e in pending_entries.iter().filter(|e| e.scope == "backend") {
        for t in &e.targets {
            if stale(e, t) && !backend_restart_ids.contains(t) {
                backend_restart_ids.push(t.clone());
            }
        }
    }
    let mut changes: Vec<Value> = activation::plan_rows(&pending_entries)
        .iter()
        .map(|e| {
            let activation_kind = if e.scope == "backend" {
                if e.targets.iter().any(|t| stale(e, t)) {
                    "restart"
                } else {
                    "next_start"
                }
            } else if up {
                "gateway_restart"
            } else {
                "next_start"
            };
            json!({
                "ts": e.ts,
                "kind": e.kind,
                "summary": e.summary,
                "scope": e.scope,
                "targets": e.targets,
                "activation": activation_kind,
            })
        })
        .collect();
    // Drift that makes the plan pending is also an ITEM in it
    // (tracker #57): the journal cannot contain it — drift's
    // inputs include journal-free state (an MCP credential's
    // presence flips the armed view with no config save) — so
    // without a synthesized item, a drift-only plan offered an
    // Apply over zero counted changes on every surface. The item
    // is derived here, at plan computation, never journaled.
    if drift {
        changes.push(activation::drift_plan_change(
            &drift_detail,
            activation::gateway_config_written_at(&state.data_dir),
        ));
    }
    let gateway_pending = drift
        || pending_entries.iter().any(|e| e.scope != "backend")
        || changes.iter().any(|c| c["activation"] == "gateway_restart");
    let actionable = gateway_pending || changes.iter().any(|c| c["activation"] == "restart");
    let admin_approval = gateway_pending && firewall_update_pending(&cfg, &state.data_dir);
    Ok(json!({
        "pending": pending,
        "gateway_up": probe_up,
        "gateway_state": view.state,
        "gateway_pending": gateway_pending,
        "drift": drift,
        "files_differ": drift_detail.any(),
        "applies_at_next_start": pending && !up,
        "actionable": actionable,
        "backend_restart_ids": backend_restart_ids,
        // Applying changes the gateway's port or network access, so the
        // firewall rule changes with it (A17).
        "admin_approval_required": admin_approval,
        "admin_approval_reason": admin_approval.then_some(
            "Windows will ask for administrator approval to update the firewall rule \
             for the gateway."
        ),
        "changes": changes,
        "snapshot_available": journal.snapshot.is_some() && !pending_entries.is_empty(),
        // The server-side activation apply in flight, null when idle:
        // every surface disables Apply and shows its progress line.
        "activation_applying": applying,
    }))
}

/// Cancel the pending activation plan's changes: restore the
/// configuration snapshot taken before the first pending change.
/// Refused when the configuration has changed through another path
/// since (the restore would discard that change too) — background
/// bookkeeping stamps excepted — or when a security-relevant section
/// would be rolled back (PS-10; field classification in
/// [`activation_cancel`]). Order: live state and the gateway probe
/// without the config lock → guard under the write lock → restore →
/// save → clear the journal; a crash between the save and the clear
/// self-heals on the next cancel ([`activation::CancelPlan::StaleJournal`]).
pub async fn op_gateway_activation_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    // Live-state resolution with the real start times, running set,
    // and gateway probe: entries whose changes are already live
    // (their backends restarted) or already effective (their
    // targets are stopped, the gateway is down) are not pending
    // and must not be "cancelled" (code-review 4.1; Pete's report,
    // 2026-10-17 — cancel was the only resolution offered for an
    // already-effective change, and it reverted the edit).
    //
    // Gathered WITHOUT the config write lock (PS-10): the supervisor
    // lock and the ~2.5 s gateway probe used to run under it,
    // stalling every config reader and writer meanwhile. The guard
    // (activation::cancel_plan) then runs under the write lock, after
    // the probe, so it judges exactly the configuration the restore
    // replaces — a change that landed during the probe is seen.
    let live_cfg = state.config.read().await.clone();
    let (probe_host, port) = gateway_endpoint(state).await;
    let started_at = backend_start_times(state, &live_cfg).await;
    let running = backend_running_ids(state, &live_cfg).await;
    drop(live_cfg);
    let up = tokio::task::spawn_blocking(move || probe_gateway(&probe_host, port))
        .await
        .unwrap_or(false);
    let mut cfg = state.config.write().await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let restored = match activation::cancel_plan(
        &state.data_dir,
        &cfg,
        &started_at,
        &configured,
        &running,
        up,
    ) {
        Ok(activation::CancelPlan::Restore(restored)) => *restored,
        Ok(activation::CancelPlan::Nothing) => {
            return Ok(json!({ "cancelled": false, "reason": "no pending changes" }))
        }
        Ok(activation::CancelPlan::StaleJournal) => {
            // Self-heal (PS-10): the configuration already equals
            // the snapshot — an earlier cancel saved the restore and
            // stopped before clearing the journal. Clearing it is
            // the only step left.
            activation::clear_journal(&state.data_dir).map_err(ApiError::InvalidInput)?;
            drop(cfg);
            {
                let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
                watch.activation_cancelled = true;
            }
            return Ok(json!({
                "cancelled": true,
                "already_restored": true,
                "reason": "the configuration already matched the pre-change snapshot; \
                           the stale pending-change record was cleared",
            }));
        }
        Err(msg) => return Err(ApiError::InvalidInput(msg)),
    };
    manager_config::validate(&restored).map_err(ApiError::from)?;
    manager_config::save(&restored, &state.config_path).map_err(ApiError::from)?;
    *cfg = restored.clone();
    drop(cfg);
    // Make the on-disk gateway YAML the one the restored config
    // generates (MCP design §10.2): a start that failed after writing
    // it left the cancelled changes there. Generated from the
    // armed-only MCP view, like every start and the drift check, and
    // written only when it differs: rewriting it from the raw registry
    // (or for nothing) made a cancelled backend-only edit show up as
    // new gateway drift (A18). A regeneration failure does NOT fail
    // the cancel — the config restore already succeeded, and the
    // next gateway start regenerates the YAML from the restored
    // config anyway — so the failure is reported in the result
    // payload instead of as a command error.
    let yaml_path = state.data_dir.join("gateway").join("config.yaml");
    let presence = mcp_credential_presence(&[&restored]).await;
    let armed =
        manager_wizard::config_with_armed_gateway_mcp_servers(&restored, |t| presence.contains(t));
    let yaml_current =
        !activation::config_drift_detail(&restored, &state.data_dir, |t| presence.contains(t)).yaml
            && yaml_path.exists();
    let yaml_error: Option<String> = match manager_gateway::generate_config_yaml(&armed) {
        Ok(_) if yaml_current => None,
        Ok(yaml) => {
            let write =
                std::fs::create_dir_all(yaml_path.parent().expect("the YAML path has a parent"))
                    .and_then(|_| std::fs::write(&yaml_path, yaml));
            write
                .err()
                .map(|e| format!("could not write {}: {e}", yaml_path.display()))
        }
        Err(e) => Some(format!("could not regenerate the gateway config: {e}")),
    };
    activation::clear_journal(&state.data_dir).map_err(ApiError::InvalidInput)?;
    ensure_backend_specs(state).await;
    // The plan's pending → resolved transition in the notification
    // monitor is a cancellation, not an application: stay silent.
    {
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        watch.activation_cancelled = true;
    }
    Ok(json!({
        "cancelled": true,
        "yaml_regenerated": yaml_error.is_none(),
        "yaml_error": yaml_error,
    }))
}

/// Mark the gateway side of the activation plan complete after a
/// successful gateway (re)start. The start-gateway step regenerates
/// everything from the live configuration, so gateway-scope
/// journaled changes are live by definition at that point (the
/// plan also self-resolves through the applied-stamp rule; this
/// just clears promptly). Backend-scope entries are NOT cleared
/// here: they resolve through their backends' (re)starts, which
/// the scoped resolution rule observes directly.
///
/// Refused (`cleared: false`) while the gateway is not running: a
/// down or failed gateway serves none of those changes (ACT-1).
/// The start-gateway step does this bookkeeping itself on success
/// ([`complete_gateway_activation`]); this op remains for clients
/// that still call it after their own start.
pub async fn op_gateway_activation_complete(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let (view, _) = running_gateway_view(state).await;
    if view.state != lifecycle::LifecycleState::Running {
        return Ok(json!({
            "cleared": false,
            "reason": "gateway is not running",
            "gateway_state": view.state,
        }));
    }
    let before = GatewayPlanBefore::capture(state).await;
    complete_gateway_activation(state, sink, lifecycle::Origin::Ui, &before).await;
    Ok(json!({ "cleared": true }))
}

/// The gateway side of the activation plan after a successful
/// gateway (re)start: clear the gateway journal scope; when the
/// start resolved the last pending change, fire the one "Activation
/// plan applied" toast (and mark it so the monitor does not repeat
/// it); and when a plan was pending before the start and the start
/// did not come from the activation apply itself (which records its
/// own execution), record the execution in the activation history
/// with the starting surface as its source (CR-6).
pub(crate) async fn complete_gateway_activation(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: lifecycle::Origin,
    before: &GatewayPlanBefore,
) {
    if let Err(e) = activation::clear_gateway_scope(&state.data_dir) {
        lifecycle::mark(
            &state.data_dir,
            lifecycle::GATEWAY,
            "activation bookkeeping failed",
            Some(&e),
            Some(origin),
        );
    }
    if !before.pending {
        return;
    }
    let (after, drift_after) = activation_unresolved(state).await;
    if !before.gateway_entries.is_empty() && after.is_empty() && !drift_after {
        // The gateway side of a plan just went live and nothing
        // remains pending: the plan is applied. When backend entries
        // are still outstanding, the monitor fires it as they resolve.
        {
            let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
            watch.activation_applied_notified = true;
        }
        notify::notify(
            sink.as_ref(),
            state,
            notify::Category::Activation,
            "Activation plan applied",
            "All saved changes are now live.",
        )
        .await;
    }
    if origin == lifecycle::Origin::Activation
        || (before.gateway_entries.is_empty() && !before.drift)
    {
        return;
    }
    let mut execution = gateway_restart_execution(origin, before, chrono::Utc::now());
    if let Some(id) = activation_history::record_execution(&state.data_dir, &mut execution) {
        lifecycle::mark(
            &state.data_dir,
            lifecycle::GATEWAY,
            "activation plan applied",
            Some(&format!(
                "{} gateway change(s), execution {id}",
                execution.items.len()
            )),
            Some(origin),
        );
    }
}

/// The activation-history record of a gateway (re)start that applied
/// the plan's gateway side: one applied item per gateway-scope entry
/// (plus a drift item when only drift was pending), sourced from the
/// starting surface. Pure.
pub(crate) fn gateway_restart_execution(
    origin: lifecycle::Origin,
    before: &GatewayPlanBefore,
    finished_at: chrono::DateTime<chrono::Utc>,
) -> activation_history::ActivationExecution {
    let mut items: Vec<activation_history::ActivationExecutionItem> =
        activation::plan_rows(&before.gateway_entries)
            .iter()
            .map(|e| activation_history::ActivationExecutionItem {
                ts: e.ts.clone(),
                kind: e.kind.clone(),
                summary: e.summary.clone(),
                scope: e.scope.clone(),
                targets: e.targets.clone(),
                activation: "gateway_restart".to_string(),
                outcome: "applied".to_string(),
                detail: None,
            })
            .collect();
    if items.is_empty() && before.drift {
        items.push(activation_history::ActivationExecutionItem {
            ts: before.started_at.unwrap_or(finished_at).to_rfc3339(),
            kind: "gateway_drift".to_string(),
            summary: "Generated gateway configuration regenerated".to_string(),
            scope: "gateway".to_string(),
            targets: Vec::new(),
            activation: "gateway_restart".to_string(),
            outcome: "applied".to_string(),
            detail: None,
        });
    }
    activation_history::ActivationExecution {
        id: String::new(),
        started_at: before.started_at.unwrap_or(finished_at).to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        status: "completed".to_string(),
        source: origin.as_str().to_string(),
        mode: "restart".to_string(),
        drift: before.drift,
        gateway_up: true,
        gateway_pending: true,
        quiesce: None,
        items,
    }
}

/// Record one finished activation plan execution in the durable
/// history ([`activation_history`]). The apply itself is
/// orchestrated by the desktop UI (`applyActivationPlan`), which
/// reports the execution whole when it ends — including an aborted
/// one, whose record is the one an operator most needs to review.
///
/// Best-effort by contract, like the bookkeeping it follows:
/// recording must never fail or alter the activation it describes,
/// so an unparseable report or a failed write answers
/// `recorded: false` instead of an error. The server assigns the
/// execution id; any id in the report is ignored.
pub async fn op_gateway_activation_record_execution(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    record: Value,
) -> ApiResult {
    let mut execution: activation_history::ActivationExecution =
        match serde_json::from_value(record) {
            Ok(e) => e,
            Err(_) => return Ok(json!({ "recorded": false })),
        };
    let id = activation_history::record_execution(&state.data_dir, &mut execution);
    Ok(json!({ "recorded": id.is_some(), "id": id }))
}

/// The activation plan execution history: with no `execution_id`,
/// the recorded executions newest-first as summaries (id, times,
/// status, source, plan state, per-outcome counts); with one, that
/// execution's full record — the plan as computed at apply time
/// with every item's outcome — or `null` when it is not in the
/// retained history. A missing or corrupt history file reads as an
/// empty history, never an error.
pub async fn op_gateway_activation_history(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    execution_id: Option<String>,
) -> ApiResult {
    let history = activation_history::load_history(&state.data_dir);
    if let Some(id) = execution_id {
        let found = history.executions.iter().rev().find(|e| e.id == id);
        return Ok(json!({ "execution": found }));
    }
    let summaries: Vec<Value> = history
        .executions
        .iter()
        .rev()
        .map(|e| {
            let (applied, skipped, failed) = e.outcome_counts();
            json!({
                "id": e.id,
                "started_at": e.started_at,
                "finished_at": e.finished_at,
                "status": e.status,
                "source": e.source,
                "mode": e.mode,
                "quiesce": e.quiesce,
                "drift": e.drift,
                "gateway_up": e.gateway_up,
                "gateway_pending": e.gateway_pending,
                "items": e.items.len(),
                "applied": applied,
                "skipped": skipped,
                "failed": failed,
            })
        })
        .collect();
    Ok(json!({ "executions": summaries }))
}

/// What a timed-out drain says in the execution record.
pub(crate) fn drain_timeout_detail(quiesce: &Value) -> String {
    format!(
        "The drain timed out with {} request(s) still in flight; the gateway was restarted anyway.",
        quiesce["in_flight"].as_f64().unwrap_or(0.0)
    )
}

/// Quiesce wait: poll the gateway's in-flight request gauge (less the
/// poll's own request, [`activation::client_requests_in_flight`]) until
/// it reaches zero or the timeout expires. A gateway that does not
/// answer metrics (down, or the metrics config not yet active)
/// counts as drained — nothing can be in flight through a gateway
/// that is not serving, and the gauge series only exists once
/// traffic has flowed.
pub async fn op_gateway_quiesce_wait(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    timeout_secs: u64,
) -> ApiResult {
    Ok(gateway_quiesce(state, timeout_secs).await)
}

/// The quiesce wait behind [`op_gateway_quiesce_wait`] and the drain
/// phase of [`op_gateway_restart`]: `{drained, in_flight, timed_out}`.
pub(crate) async fn gateway_quiesce(state: &AppState, timeout_secs: u64) -> Value {
    let timeout = timeout_secs.clamp(5, 600);
    let (host, port) = gateway_endpoint(state).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut last_in_flight: Option<f64>;
    loop {
        let key = RealSystemOps::default()
            .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
            .unwrap_or_default();
        let body = if key.is_empty() {
            None
        } else {
            let (h, k) = (host.clone(), key);
            tokio::task::spawn_blocking(move || fetch_gateway_metrics(&h, port, &k))
                .await
                .ok()
                .flatten()
        };
        match body {
            Some(text) => match activation::client_requests_in_flight(&text) {
                Some(0.0) | None => {
                    return json!({ "drained": true, "in_flight": 0, "timed_out": false })
                }
                Some(n) => last_in_flight = Some(n),
            },
            None => return json!({ "drained": true, "in_flight": 0, "timed_out": false }),
        }
        if std::time::Instant::now() >= deadline {
            return json!({
                "drained": false,
                "in_flight": last_in_flight.unwrap_or(0.0),
                "timed_out": true,
            });
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Restart the gateway server-side, in one operation every surface
/// shares (the Gateway view, the activation apply, the REST/CLI
/// route).
///
/// - Takes the gateway's lifecycle transition (`restarting`) first:
///   another gateway operation in flight refuses it with
///   [`ApiError::Busy`] before anything is touched (GW-8).
/// - `drain`: waits for in-flight gateway requests first (the
///   quiesce wait, up to 120 s; a timeout proceeds and is reported).
/// - Runs the start-gateway step under that transition. The step
///   prepares while the old gateway keeps serving, stops it just
///   before the spawn, and adopts the running PostgreSQL server: a
///   restart never calls [`op_gateway_stop`] and never stops
///   PostgreSQL (GW-1/GW-2; only a PostgreSQL port change restarts
///   it, inside the step).
///
/// Payload: `{ok, busy?, running_step?, outcome, message,
/// manual_command, task_id, quiesce?}` — `ok: false` for a refused,
/// failed or manual-action run (the shared outcome rule,
/// [`lifecycle::payload_failure`]).
pub async fn op_gateway_restart(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: Option<String>,
    drain: Option<bool>,
) -> ApiResult {
    let origin = op_origin(origin.as_deref());
    let drain = drain == Some(true);
    let guard = lifecycle::begin(
        state,
        sink,
        lifecycle::GATEWAY,
        lifecycle::TransitionKind::Restarting,
        origin,
        drain.then_some("drain in-flight requests first"),
    )
    .await?;
    // The database stays up across a gateway restart; say so in the
    // gateway's own log, so a reader never wonders whether it moved.
    let pg = lifecycle::postgres_view(state).await;
    if pg.state == lifecycle::LifecycleState::Running {
        lifecycle::mark(
            &state.data_dir,
            lifecycle::GATEWAY,
            "PostgreSQL kept running",
            pg.pid.map(|p| format!("pid {p}")).as_deref(),
            Some(origin),
        );
    }
    let quiesce = if drain {
        lifecycle::mark(
            &state.data_dir,
            lifecycle::GATEWAY,
            "draining in-flight requests",
            None,
            Some(origin),
        );
        let q = gateway_quiesce(state, 120).await;
        let detail = if q["timed_out"] == json!(true) {
            drain_timeout_detail(&q)
        } else {
            "drained".to_string()
        };
        lifecycle::mark(
            &state.data_dir,
            lifecycle::GATEWAY,
            "drain finished",
            Some(&detail),
            Some(origin),
        );
        Some(q)
    } else {
        None
    };
    let run =
        run_wizard_step_inner(state, sink, "start-gateway", origin, Some(guard), None).await?;
    let mut payload = gateway_restart_payload(&run);
    if let Some(q) = quiesce {
        payload["quiesce"] = q;
    }
    Ok(payload)
}

/// The [`op_gateway_restart`] payload of one start-gateway run.
pub(crate) fn gateway_restart_payload(run: &WizardRun) -> Value {
    if let Some(busy) = &run.busy {
        let mut p = busy.clone();
        p["ok"] = json!(false);
        if p.get("message").is_none() {
            p["message"] = json!(match busy.get("running_step").and_then(Value::as_str) {
                Some(step) => format!("busy: {step} is running"),
                None => "busy: another setup step is running".to_string(),
            });
        }
        return p;
    }
    let outcome = run.outcome.as_ref();
    json!({
        "ok": run.ok(),
        "task_id": run.task_id,
        "outcome": outcome,
        "message": outcome.map(|o| o.message.clone()),
        "manual_command": outcome.and_then(|o| o.manual_command.clone()),
        // An activation plan was pending when the restart began (the
        // completion bookkeeping ran on success).
        "activation_pending_before": run.pending_before,
    })
}

/// Stop the gateway — the gateway only, by default.
///
/// - Takes the gateway's lifecycle transition (`stopping`); another
///   gateway operation in flight refuses it with [`ApiError::Busy`].
/// - Records intent Stopped (bumping the lifecycle generation) BEFORE
///   the kill, so no monitor tick reads the exit as a crash (GW-6).
/// - Service route: the SCM stop through the shared
///   [`manager_services::stop_and_wait`]. Process route:
///   [`manager_wizard::stop_gateway_and_wait`] — finds the gateway by
///   its pid file, else its port, stops it gracefully, and confirms
///   the exit and the port release (CR-4).
/// - Markers in the gateway's own log: "stop requested by …", then
///   "stopped: pid N, found by pid file|port owner" or "stopped: was
///   not running".
/// - PostgreSQL is stopped ONLY when `stop_postgres == Some(true)`
///   (an explicit "stop gateway and database"), under its own
///   lifecycle transition and markers (GW-1).
///
/// Payload: `{ok, stopped, pid, found_by, postgres_stopped}`.
pub async fn op_gateway_stop(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    stop_postgres: Option<bool>,
    origin: Option<String>,
) -> ApiResult {
    let origin = op_origin(origin.as_deref());
    let guard = lifecycle::begin(
        state,
        sink,
        lifecycle::GATEWAY,
        lifecycle::TransitionKind::Stopping,
        origin,
        None,
    )
    .await?;
    state
        .lifecycle
        .set_intent(lifecycle::GATEWAY, lifecycle::Intent::Stopped);
    let (_, port) = gateway_endpoint(state).await;
    let data_dir = state.data_dir.clone();
    let result: Result<(bool, Option<u32>, Option<&'static str>), String> = if state
        .service_route_active(manager_services::GATEWAY_SERVICE_NAME)
        .await
    {
        // Service mode: the SCM owns the gateway process; a pid kill
        // would take the service host's child out from under it.
        let pid = lifecycle::scm_status(
            state,
            manager_services::GATEWAY_SERVICE_NAME,
            Duration::from_secs(1),
        )
        .await
        .filter(lifecycle::scm_live)
        .and_then(|s| s.child_pid.or(s.pid));
        tokio::task::spawn_blocking(move || {
            let name = manager_services::GATEWAY_SERVICE_NAME;
            let control = manager_services::scm::platform_control();
            manager_services::stop_and_wait(
                &control,
                name,
                manager_services::ServiceTimeouts::stop(name),
            )
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("gateway stop task failed: {e}"))
        .and_then(|r| r)
        .map(|outcome| match outcome {
            manager_services::StopOutcome::Stopped { .. } => (true, pid, Some("service")),
            manager_services::StopOutcome::AlreadyStopped
            | manager_services::StopOutcome::NotRegistered => (false, None, None),
        })
    } else {
        tokio::task::spawn_blocking(move || {
            manager_wizard::stop_gateway_and_wait(&data_dir, port, true)
        })
        .await
        .map_err(|e| format!("gateway stop task failed: {e}"))
        .and_then(|r| r)
        .map(|o| {
            let by = match o.found_by {
                manager_wizard::FoundBy::PidFile => Some("pid_file"),
                manager_wizard::FoundBy::PortOwner => Some("port_owner"),
                manager_wizard::FoundBy::None => None,
            };
            (o.stopped, o.pid, by)
        })
    };
    let (stopped, pid, found_by) = match result {
        Ok(r) => r,
        Err(e) => {
            lifecycle::finish(state, sink.as_ref(), guard, Err(e.clone()));
            return Err(ApiError::Gateway(e));
        }
    };
    let detail = match (stopped, pid, found_by) {
        (true, Some(pid), Some("service")) => format!("pid {pid}, service stop"),
        (true, Some(pid), Some(by)) => format!("pid {pid}, found by {}", by.replace('_', " ")),
        (true, _, _) => "service stopped".to_string(),
        (false, _, _) => "was not running".to_string(),
    };
    lifecycle::finish(state, sink.as_ref(), guard, Ok(detail));
    let postgres_stopped = if stop_postgres == Some(true) {
        postgres_stop_inner(state, sink, origin).await?
    } else {
        false
    };
    Ok(json!({
        "ok": true,
        "stopped": stopped,
        "pid": pid,
        "found_by": found_by,
        "postgres_stopped": postgres_stopped,
    }))
}

/// A start-gateway that finished Done has the app-managed PostgreSQL
/// server up again (adopted or started by `ensure_postgres_running`):
/// an earlier "Stop database" no longer stands, so PostgreSQL is
/// expected up again (intent Running, the stale failure cleared) and a
/// later postmaster death reads `failed` and is reported by the monitor.
/// Harmless without an app-managed server (its view reads
/// `not_installed` before intent is consulted).
pub(crate) fn rearm_postgres_after_gateway_start(state: &AppState) {
    state
        .lifecycle
        .set_intent(lifecycle::POSTGRES, lifecycle::Intent::Running);
    state.lifecycle.clear_failure(lifecycle::POSTGRES);
}

/// Stop the app-managed PostgreSQL server under its own lifecycle
/// transition (`pg_ctl stop -m fast`; the wizard writes the pg_ctl
/// markers into `postgres.lifecycle.log`). Intent Stopped is recorded
/// before the stop. Returns whether a running server was stopped.
pub(crate) async fn postgres_stop_inner(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: lifecycle::Origin,
) -> Result<bool, ApiError> {
    let guard = lifecycle::begin(
        state,
        sink,
        lifecycle::POSTGRES,
        lifecycle::TransitionKind::Stopping,
        origin,
        None,
    )
    .await?;
    state
        .lifecycle
        .set_intent(lifecycle::POSTGRES, lifecycle::Intent::Stopped);
    match manager_wizard::steps::stop_postgres_server(&RealSystemOps::default(), &state.data_dir)
        .await
    {
        Ok(stopped) => {
            lifecycle::finish(
                state,
                sink.as_ref(),
                guard,
                Ok(if stopped {
                    String::new()
                } else {
                    "was not running".to_string()
                }),
            );
            Ok(stopped)
        }
        Err(e) => {
            let msg = e.to_string();
            lifecycle::finish(state, sink.as_ref(), guard, Err(msg.clone()));
            Err(ApiError::Gateway(format!("PostgreSQL stop failed: {msg}")))
        }
    }
}

/// Stop the app-managed PostgreSQL server on its own. Refused while
/// the gateway is live or has an operation in flight (the gateway
/// would lose its database mid-request): stop the gateway first, or
/// use `gateway_stop` with `stop_postgres`. Payload: `{ok, stopped}`.
pub async fn op_postgres_stop(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: Option<String>,
) -> ApiResult {
    let (view, _) = running_gateway_view(state).await;
    if view.state.is_live() || view.transition.is_some() {
        return Err(ApiError::InvalidInput(format!(
            "stop the gateway first (it is {})",
            view.state
        )));
    }
    let stopped = postgres_stop_inner(state, sink, op_origin(origin.as_deref())).await?;
    Ok(json!({ "ok": true, "stopped": stopped }))
}

/// Where the RUNNING gateway listens, `(probe host, port)`. A port or
/// bind address saved while it runs takes effect at its next start, so
/// until then it answers where the last start put it: the recorded
/// spawn's endpoint when that answers, else the saved one.
pub(crate) async fn gateway_endpoint(state: &AppState) -> (String, u16) {
    let saved = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let Some(spawn) = manager_wizard::recorded_gateway_spawn(&state.data_dir) else {
        return saved;
    };
    let recorded = (
        manager_wizard::steps::probe_host_for_bind(&spawn.fingerprint.bind_host),
        spawn.fingerprint.port,
    );
    if recorded == saved {
        return saved;
    }
    let (host, port) = recorded.clone();
    match tokio::task::spawn_blocking(move || probe_gateway(&host, port)).await {
        Ok(true) => recorded,
        _ => saved,
    }
}

/// [`lifecycle::gateway_view`] judged at the running gateway's endpoint
/// ([`gateway_endpoint`]), and the port that endpoint has.
pub(crate) async fn running_gateway_view(state: &AppState) -> (lifecycle::GatewayView, u16) {
    let (_, port) = gateway_endpoint(state).await;
    if port != state.config.read().await.gateway.port {
        // It just answered there; the view probes the saved port.
        state.lifecycle.set_probe(lifecycle::GATEWAY, true);
    }
    (lifecycle::gateway_view(state).await, port)
}

/// Whether applying the saved gateway changes updates the Windows
/// firewall rule: the port or bind address changes and one side of the
/// change is reachable from the network.
pub(crate) fn firewall_update_pending(cfg: &AppConfig, data_dir: &std::path::Path) -> bool {
    let (port, bind) = manager_wizard::gateway_listen_pending(cfg, data_dir);
    let loopback = |host: &str| host == "127.0.0.1";
    (port || bind)
        && !(loopback(&cfg.gateway.bind_host)
            && manager_wizard::recorded_gateway_spawn(data_dir)
                .is_some_and(|s| loopback(&s.fingerprint.bind_host)))
}

/// One-shot gateway liveness probe: connect to <host>:<port> with a
/// 500 ms timeout, `GET /health/liveliness` with a 1 s read timeout,
/// and report whether the reply's first line is a syntactically valid
/// HTTP status line. Any status code counts — 200, 401, and 404 all
/// prove a live HTTP listener, which is all "up" means here. Any
/// error (refused, timeout, malformed reply) is simply "not up";
/// runs on a blocking thread and never fails the caller. The host is
/// the app's probe host for the configured bind address (loopback
/// whenever the bind covers it).
///
/// The path is LiteLLM's unauthenticated liveliness endpoint, not
/// `/v1/models`: this probe fires on every gateway_status poll
/// (~15 s), and the keyless `/v1/models` requests filled the gateway
/// log with "No api key passed in" auth rejections — noise that was
/// later mistaken for a startup failure's cause (2026-10-16).
pub(crate) fn probe_gateway(host: &str, port: u16) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    let addr = std::net::SocketAddr::new(ip, port);
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
    if stream
        .write_all(
            b"GET /health/liveliness HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).is_err() {
        return false;
    }
    // "HTTP/1.1 200 OK" — a valid status line starts with "HTTP/" and
    // carries a numeric status code as its second token.
    status_line.starts_with("HTTP/")
        && status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .is_some()
}

/// The activation plan's current unresolved entries and drift flag —
/// the same computation `gateway_status` reports, shared by the
/// notification monitor and the activation hooks.
pub(crate) async fn activation_unresolved(
    state: &AppState,
) -> (Vec<activation::ActivationEntry>, bool) {
    // A clone, not a held read guard (PS-11): the start times below
    // wait on the supervisor, and a read guard held across that wait
    // queues every config writer (and, behind it, every later reader).
    let cfg = state.config.read().await.clone();
    let journal = activation::load_journal(&state.data_dir);
    let started_at = backend_start_times(state, &cfg).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let unresolved =
        activation::unresolved_entries_scoped(&state.data_dir, &journal, &started_at, &configured);
    let presence = mcp_credential_presence(&[&cfg]).await;
    let drift = activation::config_drift(&cfg, &state.data_dir, |t| presence.contains(t));
    (unresolved, drift)
}

#[cfg(test)]
mod tests;
