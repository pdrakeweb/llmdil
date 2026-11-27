//! Step 12: smoke-test inference.

use super::*;

// ---------------------------------------------------------------------------
// 12. Smoke-test inference
// ---------------------------------------------------------------------------

/// Sanity bound for a smoke-test request (non-streaming proxy for TTFT).
pub const SMOKE_MAX_LATENCY_MS: u64 = 120_000;

/// Step 12: tagged test request per gateway group through the gateway;
/// assert non-empty completion + sane latency; record baselines.
pub struct SmokeTest {
    pub max_latency_ms: u64,
    /// Install mode only: how long to wait for each enabled member
    /// backend to answer its health endpoint before probing the
    /// gateway (llama-server needs time to load its model; the wizard
    /// starts the backends right before this step).
    pub ready_budget_ms: u64,
    /// Delay between health polls while waiting.
    pub ready_poll_ms: u64,
}

impl Default for SmokeTest {
    fn default() -> Self {
        Self {
            max_latency_ms: SMOKE_MAX_LATENCY_MS,
            ready_budget_ms: 240_000,
            ready_poll_ms: 2_000,
        }
    }
}

impl SmokeTest {
    fn baselines_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("wizard").join("smoke-baselines.json")
    }

    /// Wait until every enabled backend a gateway group routes to
    /// answers `GET /health` with 200 (llama-server returns 503 while
    /// its model loads and refuses connections before it binds).
    /// Returns Some(failed outcome) when a backend does not become
    /// healthy inside the budget; None when probing may proceed.
    /// The probe request body. The probe verifies routing and
    /// generation, not reasoning: thinking models (the planner)
    /// otherwise spend the whole token budget inside the think
    /// block and return an empty visible completion (found on real
    /// hardware 2026-10-15 — with max_tokens at 8, all 8 generated
    /// tokens were reasoning and the step reported "empty
    /// completion" against a healthy backend). Thinking is disabled
    /// for the probe; templates that do not know the flag ignore it.
    fn probe_body(group: &str, tag: &str) -> String {
        serde_json::json!({
            "model": group,
            "messages": [{ "role": "user", "content": format!("wizard smoke test {tag}") }],
            "max_tokens": 128,
            "stream": false,
            "chat_template_kwargs": { "enable_thinking": false },
        })
        .to_string()
    }

    async fn wait_for_backends(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Option<StepOutcome> {
        let mut targets: Vec<(String, u16)> = Vec::new();
        for group in &ctx.config.gateway.groups {
            for member in &group.members {
                if let Some(b) = ctx.config.backends.iter().find(|b| &b.id == member) {
                    if b.enabled && !targets.iter().any(|(id, _)| id == &b.id) {
                        targets.push((b.id.clone(), b.port));
                    }
                }
            }
        }
        for (id, port) in targets {
            let url = format!("http://127.0.0.1:{port}/health");
            let start = std::time::Instant::now();
            // A model load can take most of the budget; the wait
            // announces itself and reports in about every 30 s on
            // the live sink instead of sitting silent.
            push_log(
                ctx,
                log,
                format!(
                    "waiting for backend '{id}' on :{port} to become healthy (model load; up to {} s)",
                    self.ready_budget_ms / 1000
                ),
            );
            let mut last_tick_s = 0u64;
            loop {
                if let Ok(resp) = ctx.ops.http_get(&url).await {
                    if resp.status == 200 {
                        push_log(
                            ctx,
                            log,
                            format!(
                                "backend '{id}' healthy on :{port} after {}",
                                fmt_elapsed(start)
                            ),
                        );
                        break;
                    }
                }
                if start.elapsed().as_millis() as u64 >= self.ready_budget_ms {
                    return Some(StepOutcome::failed(
                        format!(
                            "backend '{id}' did not become healthy within {} s; start it from the Backends view, then re-run smoke-test",
                            self.ready_budget_ms / 1000
                        ),
                        log.clone(),
                    ));
                }
                let elapsed_s = start.elapsed().as_secs();
                if elapsed_s >= last_tick_s + 30 {
                    last_tick_s = elapsed_s;
                    ctx.emit_log(&format!(
                        "still waiting for backend '{id}' on :{port} — {elapsed_s}s elapsed"
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(self.ready_poll_ms.max(1)))
                    .await;
            }
        }
        None
    }
}

#[async_trait]
impl WizardStep for SmokeTest {
    fn id(&self) -> &str {
        STEP_SMOKE_TEST
    }
    fn name(&self) -> &str {
        "Smoke-test inference"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // The probes are HTTP POSTs, surfaced here for transparency.
        ctx.config
            .gateway
            .groups
            .iter()
            .map(|g| {
                PlannedCommand::new(vec![
                    "http-post".to_string(),
                    format!(
                        "http://{}:{}/v1/chat/completions",
                        gateway_probe_host(&ctx.config),
                        ctx.config.gateway.port
                    ),
                    Self::probe_body(&g.name, "wizard-smoke"),
                ])
            })
            .collect()
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("check backend health on the dashboard, then re-run smoke-test")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let groups: Vec<String> = ctx
            .config
            .gateway
            .groups
            .iter()
            .map(|g| g.name.clone())
            .collect();
        if groups.is_empty() {
            return Ok(StepOutcome::failed(
                "no gateway model groups configured",
                vec![],
            ));
        }
        let mut log = Vec::new();
        if ctx.mode == RunMode::Install {
            if let Some(outcome) = self.wait_for_backends(ctx, &mut log).await {
                return Ok(outcome);
            }
        }
        let tag = format!("wizard-{}", chrono::Utc::now().timestamp());
        let url = format!(
            "http://{}:{}/v1/chat/completions",
            gateway_probe_host(&ctx.config),
            ctx.config.gateway.port
        );
        // The gateway requires its master key when one is set; it is
        // read from Credential Manager, never generated here.
        let headers: Vec<(String, String)> = match ctx
            .ops
            .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
        {
            Some(key) if !key.is_empty() => {
                vec![("Authorization".to_string(), format!("Bearer {key}"))]
            }
            _ => Vec::new(),
        };
        let mut baselines = Vec::new();
        for group in &groups {
            let body = Self::probe_body(group, &tag);
            let resp = match ctx.ops.http_post_with_headers(&url, &body, &headers).await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(StepOutcome::failed(
                        format!("smoke test for '{group}' failed: {e}"),
                        vec![format!("{e:?}")],
                    ))
                }
            };
            if resp.status != 200 {
                return Ok(StepOutcome::failed(
                    format!("smoke test for '{group}': HTTP {}", resp.status),
                    vec![resp.body_str()],
                ));
            }
            let v: serde_json::Value = match serde_json::from_slice(&resp.body) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(StepOutcome::failed(
                        format!("unparseable completion for '{group}': {e}"),
                        log,
                    ))
                }
            };
            let mut content = v
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // A response that is entirely reasoning still proves
            // routing and generation (the server splits reasoning
            // into its own field, and a template that ignored
            // enable_thinking=false leaves the visible completion
            // empty within the token budget).
            let mut reasoning_only = false;
            if content.is_empty() {
                let reasoning = v
                    .pointer("/choices/0/message/reasoning_content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !reasoning.is_empty() {
                    content = reasoning;
                    reasoning_only = true;
                }
            }
            if content.is_empty() {
                let finish = v
                    .pointer("/choices/0/finish_reason")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                return Ok(StepOutcome::failed(
                    format!("smoke test for '{group}': empty completion (finish_reason: {finish})"),
                    log,
                ));
            }
            if resp.elapsed_ms > self.max_latency_ms {
                return Ok(StepOutcome::failed(
                    format!(
                        "smoke test for '{group}': latency {} ms exceeded sanity bound {} ms",
                        resp.elapsed_ms, self.max_latency_ms
                    ),
                    log,
                ));
            }
            if reasoning_only {
                log.push(format!(
                    "{group}: {} ms, reasoning-only response ({} chars)",
                    resp.elapsed_ms,
                    content.chars().count()
                ));
            } else {
                log.push(format!(
                    "{group}: {} ms, {} chars",
                    resp.elapsed_ms,
                    content.chars().count()
                ));
            }
            baselines.push(serde_json::json!({
                "group": group,
                "latency_ms": resp.elapsed_ms,
                "chars": content.chars().count(),
            }));
        }
        let doc = serde_json::json!({
            "tag": tag,
            "baselines": baselines,
            "recorded_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = self.baselines_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(&path, json_pretty(&doc).as_bytes())
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("baselines written to {}", path.display()));
        Ok(StepOutcome::done(
            format!("smoke test passed for {} group(s)", groups.len()),
            log,
        ))
    }
}
