//! Benchmark prompt generation (consulting a backend for prompts).

use super::*;

// ---------------------------------------------------------------------------
// Prompt generation (§4.3): the consultation transport aimed at
// the best running backend; the draft lands UNSAVED in the
// editor. Never starts a backend; never auto-promotes.
// ---------------------------------------------------------------------------

pub(super) const GENERATE_TEMPERATURE: f32 = 0.7;
pub(super) const GENERATE_TIMEOUT: Duration = Duration::from_secs(900);

pub async fn op_bench_prompt_generate(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    purpose: String,
    description: String,
    size: String,
    backend_id: Option<String>,
) -> crate::ApiResult {
    let (target_tokens, max_tokens) = match size.as_str() {
        "short" => (500u32, 1_200u32),
        "standard" => (4_000u32, 7_000u32),
        other => {
            return Err(ApiError::InvalidInput(format!(
                "unknown prompt size '{other}' (short | standard)"
            )))
        }
    };
    if description.trim().is_empty() {
        return Err(ApiError::InvalidInput(
            "describe the prompt to generate".to_string(),
        ));
    }
    let cfg = state.config.read().await.clone();
    // Running set from the shared liveness view, exactly as the interlock
    // reads it.
    // The running set is the shared liveness view (supervisor and
    // SCM alike): a service-managed backend holding the GPU counts
    // exactly like a supervised one.
    let running: HashSet<String> = crate::lifecycle::backend_liveness(state)
        .await
        .into_values()
        .filter(|l| l.state.is_live())
        .map(|l| l.id)
        .collect();
    let backend = match &backend_id {
        Some(id) => {
            let b = cfg
                .backends
                .iter()
                .find(|b| &b.id == id)
                .ok_or_else(|| ApiError::NotFound(format!("no backend '{id}'")))?;
            if !running.contains(id) {
                return Err(ApiError::InvalidInput(format!(
                    "backend '{id}' is not running; generation never starts a backend"
                )));
            }
            b.clone()
        }
        None => {
            // Best running backend by rated context (the
            // consultation's pick, §4.3).
            let mut candidates: Vec<&BackendConfig> = cfg
                .backends
                .iter()
                .filter(|b| running.contains(&b.id))
                .collect();
            candidates.sort_by_key(|b| std::cmp::Reverse(b.flags.n_ctx));
            candidates.first().copied().cloned().ok_or_else(|| {
                ApiError::InvalidInput(
                    "no backend is running; generation never starts a backend".to_string(),
                )
            })?
        }
    };
    let task_id = state
        .record_task(
            "bench-prompt-generate",
            json!({ "backend": backend.id, "purpose": purpose, "size": size }),
        )
        .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.bench_generate_abort.lock().await;
        if slot.is_some() {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": "a prompt generation is already running" }),
                )
                .await;
            return Err(ApiError::InvalidInput(
                "a prompt generation is already running".to_string(),
            ));
        }
        *slot = Some(tx);
    }
    state
        .tasks
        .controls
        .set_canceller(&task_id, crate::tasks::TaskCanceller::BenchPromptGenerate);
    let model_name = crate::model_for_backend(&cfg, &backend)
        .map(|m| m.id.clone())
        .unwrap_or_else(|| backend.id.clone());
    let system_prompt = format!(
        "You write benchmark prompts for a local LLM benchmarking tool. \
         Output ONLY the prompt text itself — no commentary, no preamble, no markdown fences. \
         The prompt must be fully self-contained: every piece of material the model needs \
         (code, documents, data) is inside the prompt itself. \
         Purpose tag: {purpose}. Target length: about {target_tokens} tokens. \
         For a coder prompt, include real, coherent code with consistent identifiers and an \
         edit-or-continue task that deliberately reuses those identifiers. \
         For a planner prompt, state a constrained problem with explicit constraints and a \
         concrete deliverable. \
         For a tool-runner prompt, describe a scenario that calls for structured, tool-style \
         steps over concrete data included in the prompt. \
         For a general prompt, include substantive source text and a question over it."
    );
    let messages = vec![
        manager_supervisor::ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        },
        manager_supervisor::ChatMessage {
            role: "user".to_string(),
            content: description.trim().to_string(),
        },
    ];
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let completion = manager_supervisor::chat_completion(
        &base_url,
        &model_name,
        &messages,
        GENERATE_TEMPERATURE,
        max_tokens,
        GENERATE_TIMEOUT,
    );
    let outcome = tokio::select! {
        res = completion => Some(res),
        _ = rx => None,
    };
    {
        let mut slot = state.bench_generate_abort.lock().await;
        *slot = None;
    }
    match outcome {
        None => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Aborted,
                    json!({ "backend": backend.id }),
                )
                .await;
            Err(ApiError::InvalidInput(
                "prompt generation cancelled".to_string(),
            ))
        }
        Some(Err(e)) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": e.to_string() }),
                )
                .await;
            Err(ApiError::InvalidInput(format!(
                "prompt generation failed: {e}"
            )))
        }
        Some(Ok(result)) => {
            if result.content.trim().is_empty() {
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Failed,
                        json!({ "error": "the backend returned an empty draft" }),
                    )
                    .await;
                return Err(ApiError::InvalidInput(format!(
                    "backend '{}' returned an empty draft ({} transport bytes, {} reasoning bytes)",
                    backend.id, result.response_bytes, result.reasoning_bytes
                )));
            }
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Done,
                    json!({ "backend": backend.id, "chars": result.content.len() }),
                )
                .await;
            Ok(json!({
                "text": result.content,
                "backend_id": backend.id,
                "model": model_name,
            }))
        }
    }
}

pub async fn op_bench_prompt_generate_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let sender = {
        let mut slot = state.bench_generate_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}
