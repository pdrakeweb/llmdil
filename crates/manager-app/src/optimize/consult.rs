//! Optimize state / purpose / consult / discover operations.

use super::*;

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Full page state: purposes, context summary + document, the
/// stored plan with live validation states.
pub async fn op_optimize_get_state(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    state_payload(state).await
}

/// Persist a purpose text (design §2.3): the deployment purpose,
/// or one group's / backend's. A scoped config save through
/// `manager_config::save`, like every other config write.
pub async fn op_optimize_save_purpose(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    scope: String,
    target: Option<String>,
    text: String,
) -> crate::ApiResult {
    // Write lock across clone, save and install (finding PS-2,
    // docs/code-review-v0.1.60.md): cloning under a read lock and
    // installing later overwrote any config change that landed in
    // between (e.g. the download watcher's verified_at stamp).
    let mut live = state.config.write().await;
    let mut new_cfg = live.clone();
    match scope.as_str() {
        "deployment" => new_cfg.optimize_purpose = text,
        "backend" => {
            let target = target.unwrap_or_default();
            let b = new_cfg
                .backends
                .iter_mut()
                .find(|b| b.id == target)
                .ok_or_else(|| ApiError::NotFound(format!("backend {target}")))?;
            b.purpose = text;
        }
        "group" => {
            let target = target.unwrap_or_default();
            let g = new_cfg
                .gateway
                .groups
                .iter_mut()
                .find(|g| g.name == target)
                .ok_or_else(|| ApiError::NotFound(format!("group {target}")))?;
            g.purpose = text;
        }
        other => {
            return Err(ApiError::InvalidInput(format!(
                "unknown purpose scope '{other}'"
            )))
        }
    }
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *live = new_cfg;
    drop(live);
    state_payload(state).await
}

/// The refusal of an Admin API consultation that would replace the
/// stored plan (and its decisions) without saying so; the window shows
/// the plan and asks. `None` when nothing would be lost or the caller
/// passed `replace`.
pub fn consult_replace_refusal(state: &AppState, replace: Option<bool>) -> Option<String> {
    (replace != Some(true) && load_plan_file(state).plan.is_some()).then(|| {
        "a plan exists; a consultation replaces it and its decisions — pass \"replace\": true to \
         replace it, or discard it first"
            .to_string()
    })
}

/// Run (or sample) a consultation. Live mode consults the chosen
/// running backend directly over loopback as a tracked task with
/// the scoped cancellation handle; sample mode parses a locally
/// generated plan against the real context, with no backend
/// involved.
pub async fn op_optimize_consult(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    backend_id: Option<String>,
    sample: Option<bool>,
    directive: Option<String>,
    candidate_repos: Option<Vec<String>>,
    replace: Option<bool>,
) -> crate::ApiResult {
    let sample = sample.unwrap_or(false);
    // The REST layer answers this one 409 before it starts the
    // operation; checked here too, where the plan is replaced.
    if crate::event_bus::current_origin() == crate::event_bus::EventOrigin::Api {
        if let Some(why) = consult_replace_refusal(state, replace) {
            return Err(ApiError::Busy(why));
        }
    }
    // The operator directive for this consultation (addendum
    // A.2): trimmed; empty (or absent) means no directive. Sample
    // consultations record it the same way live ones do — the
    // plan records what produced it.
    let directive = directive
        .as_deref()
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    {
        let slot = state.optimize.consult_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a consultation is already running".to_string(),
            ));
        }
    }
    // A consultation replaces the plan; the Rust apply executor
    // (Phase 3) records outcomes on the stored plan as it runs.
    if state.optimize.live.applying() {
        return Err(ApiError::InvalidInput(
            "a plan apply is running; consult after it finishes".to_string(),
        ));
    }
    {
        // Cross-family serialization (Addendum B §B.4.3,
        // extension E2): a consultation refuses while the
        // Optimize job slot is occupied — by a measurement, an
        // experiment batch, or a tuning sweep — the reciprocal
        // of the check those jobs make against this slot. A
        // consultation is generation traffic; run during a
        // sweep it would contend for the CPU and system RAM
        // the sweep's arms are measuring.
        let slot = state.optimize.job_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a measurement, experiment, or tuning job is already running".to_string(),
            ));
        }
    }
    let cfg = state.config.read().await.clone();
    let mut gathered = gather(state, &cfg).await;
    // Discovery pre-pass carriage (Phase 5c, §A.5): only a
    // consultation WITH a directive receives a candidates block,
    // and only the stored candidates gathered for that same
    // directive — the consult never re-runs discovery itself;
    // the block comes from the stored, operator-visible set,
    // intersected with the repos the operator left un-pruned
    // (`candidate_repos`; absent = all stored).
    if !directive.is_empty() {
        let stored = load_candidates_file(state);
        if stored.directive == directive {
            gathered.measure.candidates = stored
                .candidates
                .into_iter()
                .filter(|c| {
                    candidate_repos
                        .as_ref()
                        .map(|repos| repos.iter().any(|r| r == &c.repo_id))
                        .unwrap_or(true)
                })
                .collect();
        }
    }
    let (default_running, best_overall) = consult_default(&gathered.candidates);
    let target = backend_id
        .clone()
        .or_else(|| default_running.clone())
        .or(best_overall)
        .or_else(|| cfg.backends.first().map(|b| b.id.clone()))
        .ok_or_else(|| ApiError::InvalidInput("no backends are configured".to_string()))?;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == target)
        .ok_or_else(|| ApiError::NotFound(format!("backend {target}")))?
        .clone();
    // Mutual exclusion (tracker #70): a live consultation claims
    // its backend against benchmark jobs for its whole run, and a
    // benchmark already running on that backend refuses the start
    // here, naming the running operation. Sample consultations
    // consult no backend, so they neither claim nor are refused.
    // The guard releases the record on every exit below —
    // completion, failure, parse failure, and the operator-cancel
    // path alike (the Oct 20, 2026 cancelled-consultation case).
    let _consult_op_guard = if sample {
        None
    } else {
        Some(OptimizeOpGuard::claim(
            state,
            &target,
            OptimizeOpFamily::Consultation,
            "consultation",
        )?)
    };
    if !sample && gathered.running.get(&target).copied() != Some(true) {
        return Err(ApiError::InvalidInput(format!(
            "backend '{target}' is not running; consultations go to a running backend"
        )));
    }
    let (context, meta) =
        build_context(&cfg, &gathered.measure, env!("CARGO_PKG_VERSION"), &target);
    let purpose = if cfg.optimize_purpose.trim().is_empty() {
        default_purpose_seed(&cfg)
    } else {
        cfg.optimize_purpose.clone()
    };
    let (system_prompt, user_prompt) = build_prompts(&purpose, &directive, &context, &meta);

    // Optimize log (tracker #52): the consultation's start and
    // the manifest of what the consulted model is about to be
    // told. The directive is the operator's own input.
    {
        let mut lines = vec![opt_log_line(&format!(
            "consultation start: backend '{target}'{}; directive: {}",
            if sample { " (sample)" } else { "" },
            if directive.is_empty() {
                "(none)".to_string()
            } else {
                format!("'{directive}'")
            }
        ))];
        lines.extend(context_manifest_lines(
            &context,
            &system_prompt,
            &user_prompt,
        ));
        opt_log(state, &lines);
    }

    // Context-fit pre-check (design §3): the assembled prompt plus
    // the response budget must fit the consulted model's context.
    if !sample {
        let est = estimate_tokens(&system_prompt) + estimate_tokens(&user_prompt);
        let n_ctx = backend.flags.n_ctx as usize;
        if est + CONSULT_MAX_TOKENS as usize > n_ctx {
            let msg = format!(
                "the assembled context is ~{est} tokens (estimated at 4 characters per token); with the {CONSULT_MAX_TOKENS}-token response budget it needs {} tokens, over backend '{target}'s configured context of {n_ctx} — pick a backend with a larger context",
                est + CONSULT_MAX_TOKENS as usize
            );
            opt_log1(state, &format!("consultation refused: {msg}"));
            return Err(ApiError::InvalidInput(msg));
        }
    }

    let task_id = state
        .record_task(
            "optimize-consult",
            json!({ "backend": target, "sample": sample }),
        )
        .await;

    let response_text: String = if sample {
        record_consult_start(state).await;
        sample_response(&cfg)
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        {
            let mut slot = state.optimize.consult_abort.lock().await;
            if slot.is_some() {
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Failed,
                        json!({ "error": "a consultation is already running" }),
                    )
                    .await;
                return Err(ApiError::InvalidInput(
                    "a consultation is already running".to_string(),
                ));
            }
            *slot = Some(tx);
        }
        state
            .tasks
            .controls
            .set_canceller(&task_id, crate::tasks::TaskCanceller::OptimizeConsult);
        record_consult_start(state).await;
        let model_name = crate::model_for_backend(&cfg, &backend)
            .map(|m| m.id.clone())
            .unwrap_or_else(|| backend.id.clone());
        let messages = vec![
            manager_supervisor::ChatMessage {
                role: "system".to_string(),
                content: system_prompt,
            },
            manager_supervisor::ChatMessage {
                role: "user".to_string(),
                content: user_prompt,
            },
        ];
        // Loopback by construction (design §3): the URL is a
        // literal loopback host with the backend's own port —
        // never the gateway, never the LAN address.
        let base_url = format!("http://127.0.0.1:{}", backend.port);
        // Phase 3: streamed generation with live progress
        // (`consult_progress` in the state payload) and the plan
        // contract's JSON schema as `response_format`; the client
        // falls back to weaker request shapes when this build
        // rejects them, and the salvage parser below reads the
        // answer either way.
        let response_format = manager_optimize::plan_response_format();
        let live = &state.optimize.live;
        live.begin_consult(&target);
        let _progress_guard = crate::optimize_stream::ConsultProgressGuard(live);
        let on_progress = |p: &manager_supervisor::StreamProgress| live.record(p);
        let completion = manager_supervisor::consult_completion(
            &base_url,
            &model_name,
            &messages,
            CONSULT_TEMPERATURE,
            CONSULT_MAX_TOKENS,
            Some(&response_format),
            CONSULT_TIMEOUT,
            &on_progress,
        );
        // Either cancel route stops the generation: the scoped
        // abort handle (`optimize_cancel_consult`) or the task
        // system's token (`task_abort` routes to the same op, and
        // the token covers any future direct cancel).
        let token = state.tasks.controls.token(&task_id);
        let outcome = tokio::select! {
            res = completion => Some(res),
            _ = rx => None,
            _ = token.cancelled() => None,
        };
        {
            let mut slot = state.optimize.consult_abort.lock().await;
            *slot = None;
        }
        match outcome {
            None => {
                opt_log1(
                    state,
                    &format!("consultation cancelled by operator (backend '{target}')"),
                );
                record_consult_failure(state, "consultation cancelled by operator".to_string())
                    .await;
                state
                    .finish_task(&task_id, TaskStatus::Aborted, json!({ "backend": target }))
                    .await;
                return Err(ApiError::InvalidInput("consultation cancelled".to_string()));
            }
            Some(Err(e)) => {
                opt_log1(
                    state,
                    &format!("consultation failed: backend '{target}' error: {e}"),
                );
                record_consult_failure(state, format!("consultation failed: {e}")).await;
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Failed,
                        json!({ "error": e.to_string() }),
                    )
                    .await;
                return Err(ApiError::InvalidInput(format!("consultation failed: {e}")));
            }
            Some(Ok(consult)) => {
                // The fallback trail (Phase 3): which request
                // shapes this build rejected before one was
                // accepted, and how the answer finally came back.
                let mut lines: Vec<String> = consult
                    .fallbacks
                    .iter()
                    .map(|f| opt_log_line(&format!("consultation fallback: {f}")))
                    .collect();
                lines.push(opt_log_line(&format!(
                    "consultation transport: streamed {}, response_format {}",
                    consult.streamed,
                    if consult.structured {
                        "json_schema"
                    } else {
                        "none"
                    }
                )));
                opt_log(state, &lines);
                let result = consult.result;
                // Response diagnostics (Oct 19, 2026): the raw
                // response log below records the extracted answer
                // text only, which made a "0 bytes" response
                // undiagnosable — the distinction between a body
                // that never arrived, an extraction that found
                // nothing, and a budget spent on reasoning lives
                // in these figures.
                opt_log1(
                    state,
                    &format!(
                        "consultation response: backend '{target}' endpoint {base_url}/v1/chat/completions status {} transport {} bytes, content {} bytes, reasoning {} bytes, finish_reason {}, usage prompt_tokens {} completion_tokens {}, latency {} ms",
                        result.http_status,
                        result.response_bytes,
                        result.content.len(),
                        result.reasoning_bytes,
                        result.finish_reason.as_deref().unwrap_or("absent"),
                        result.prompt_tokens.map(|n| n.to_string()).unwrap_or_else(|| "absent".to_string()),
                        result.completion_tokens.map(|n| n.to_string()).unwrap_or_else(|| "absent".to_string()),
                        result.latency_ms,
                    ),
                );
                result.content
            }
        }
    };

    // The FULL raw response, before parsing (tracker #52): when a
    // plan comes back wrong, this plus the manifest and the
    // verdicts below are the whole diagnosis.
    {
        let mut lines = vec![opt_log_line(&format!(
            "raw consultation response from '{target}' ({} bytes):",
            response_text.len()
        ))];
        for line in response_text.lines() {
            lines.push(opt_log_line(line));
        }
        opt_log(state, &lines);
    }

    // Denial memory: everything denied so far — the stored set
    // plus the outgoing plan's denied items — pre-denies matches
    // in the new plan (§6).
    let mut file = load_plan_file(state);
    let mut denied: HashSet<String> = file.denied_fingerprints.iter().cloned().collect();
    if let Some(old) = &file.plan {
        for item in &old.items {
            if item.decision == Decision::Denied {
                denied.insert(item.fingerprint.clone());
            }
        }
    }
    let config_hash = current_config_hash(&cfg);
    let plan = match parse_plan(
        &response_text,
        &cfg,
        &target,
        &config_hash,
        context,
        &denied,
        &directive,
        chrono::Utc::now(),
    ) {
        Ok(p) => p,
        Err(e) => {
            opt_log1(state, &format!("parse failed: {e}"));
            record_consult_failure(
                state,
                format!("the backend's response could not be parsed: {e}"),
            )
            .await;
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": format!("unparseable response: {e}") }),
                )
                .await;
            return Err(ApiError::InvalidInput(format!(
                "the backend's response could not be parsed: {e} — the raw response is in the Optimize log (Logs view, scope 'optimize'); try again or consult a different backend"
            )));
        }
    };
    // Per-item parse results, plan-build exclusions (tracker
    // #51), and per-item validation verdicts WITH reasons
    // (tracker #52) — the verdict lines are what would have
    // diagnosed a dead-end plan item in one look.
    {
        let mut lines = vec![opt_log_line(&format!(
            "parse result: {} items, {} excluded at plan build, {} dropped invalid, {} experiments",
            plan.items.len(),
            plan.excluded_items.len(),
            plan.dropped_invalid,
            plan.experiments.len()
        ))];
        for item in &plan.items {
            lines.push(opt_log_line(&format!(
                "parse item {}: {:?} target={} decision={:?} fingerprint={}",
                item.id,
                item.kind,
                item.target.as_deref().unwrap_or("-"),
                item.decision,
                item.fingerprint
            )));
        }
        for ex in &plan.excluded_items {
            lines.push(opt_log_line(&format!(
                "excluded at plan build: {:?} target={} — {}",
                ex.kind,
                ex.target.as_deref().unwrap_or("-"),
                ex.reason
            )));
        }
        let verdicts = validate_plan(&plan, &cfg, &gathered.live);
        for (item, v) in plan.items.iter().zip(verdicts.iter()) {
            lines.push(opt_log_line(&format!(
                "validation: item {} ({:?} {}) -> {:?}{}",
                item.id,
                item.kind,
                item.target.as_deref().unwrap_or("-"),
                v.status,
                v.reason
                    .as_ref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            )));
        }
        opt_log(state, &lines);
    }
    for item in &plan.items {
        if item.decision == Decision::Denied {
            denied.insert(item.fingerprint.clone());
        }
    }
    let item_count = plan.items.len();
    file.plan = Some(plan);
    file.sample = sample;
    file.denied_fingerprints = denied.into_iter().collect();
    save_plan_file(state, &file).map_err(ApiError::InvalidInput)?;
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "backend": target, "items": item_count }),
        )
        .await;
    record_consult_success(state).await;
    state_payload(state).await
}

/// Run the discovery pre-pass for a directive (Phase 5c,
/// §A.5 option (b)): HF API search seeded from the directive,
/// per-repo model records, and fit verdicts computed against
/// the live telemetry free-VRAM snapshot from `gather()`.
/// The result replaces the stored candidate set in
/// `<data_dir>/optimize-candidates.json`; nothing is consulted
/// and no configuration changes. A later consultation with
/// the same directive receives the (operator-pruned) set as
/// its context's `candidates` block.
pub async fn op_optimize_discover(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    directive: String,
) -> crate::ApiResult {
    let directive = directive.trim().to_string();
    if directive.is_empty() {
        return Err(ApiError::InvalidInput(
            "candidate discovery needs a directive: enter one to seed the search".to_string(),
        ));
    }
    opt_log1(state, &format!("discovery start: directive '{directive}'"));
    let task_id = state
        .record_task("optimize-discover", json!({ "directive": directive }))
        .await;
    let token = state.tasks.controls.token(&task_id);
    let outcome = tokio::select! {
        res = discover(state, directive) => res,
        _ = token.cancelled() => {
            opt_log1(state, "discovery cancelled");
            Err(ApiError::InvalidInput("candidate discovery cancelled".to_string()))
        }
    };
    let (status, detail) = match &outcome {
        Ok(count) => (TaskStatus::Done, json!({ "candidates": count })),
        Err(e) => (TaskStatus::Failed, json!({ "error": e.to_string() })),
    };
    state.finish_task(&task_id, status, detail).await;
    outcome?;
    state_payload(state).await
}

/// The body of [`op_optimize_discover`]; returns the candidate count.
async fn discover(
    state: &std::sync::Arc<crate::AppState>,
    directive: String,
) -> Result<usize, ApiError> {
    let cfg = state.config.read().await.clone();
    let gathered = gather(state, &cfg).await;
    let free_vram = gathered.live.free_vram_mib_per_gpu.clone();
    let retrieved_at = chrono::Utc::now().to_rfc3339();
    let client = HfClient::with_default_base().map_err(|e| {
        opt_log1(state, &format!("discovery failed: {e}"));
        ApiError::InvalidInput(format!("candidate discovery failed: {e}"))
    })?;
    let discovery = client
        .discover_deep(&directive, &free_vram, &retrieved_at)
        .await
        .map_err(|e| {
            opt_log1(state, &format!("discovery failed: {e}"));
            ApiError::InvalidInput(format!("candidate discovery failed: {e}"))
        })?;
    let candidates = discovery.candidates;
    let funnel = discovery.funnel;
    {
        let mut lines = vec![
            opt_log_line(&format!(
                "discovery result: {} candidates",
                candidates.len()
            )),
            opt_log_line(&funnel_log_line(&funnel)),
        ];
        for c in &candidates {
            lines.push(opt_log_line(&format!(
                "discovery candidate: {}/{} params_b={} size_bytes={} fit={}",
                c.repo_id, c.file, c.params_b, c.size_bytes, c.fit
            )));
        }
        opt_log(state, &lines);
    }
    let count = candidates.len();
    save_candidates_file(
        state,
        &CandidatesFile {
            directive,
            retrieved_at,
            candidates,
            funnel: Some(funnel),
        },
    )
    .map_err(ApiError::InvalidInput)?;
    Ok(count)
}

/// Cancel the running consultation (design §1.2): fires the
/// scoped abort handle, which drops the in-flight request.
pub async fn op_optimize_cancel_consult(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let sender = {
        let mut slot = state.optimize.consult_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            // Mutual exclusion (tracker #70): the cancel clears
            // the consultation's per-backend record now, so a
            // benchmark may start on that backend immediately —
            // the cancelled consultation's own release then finds
            // its record gone (or a newer claim) and leaves it.
            clear_running_ops(state, OptimizeOpFamily::Consultation);
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}
