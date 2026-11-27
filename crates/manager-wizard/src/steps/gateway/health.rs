//! `StartGateway`: post-start MCP load check and health confirmation.

use super::*;

impl StartGateway {
    /// Post-start MCP load check (design §8, §11, §14.6): with the
    /// master key, ask the running gateway which MCP servers it
    /// registered (`GET /v1/mcp/server`) and list each armed
    /// server's tools (`GET /mcp-rest/tools/list?server_id=<id>`).
    /// A server passes when it appears in the registered list AND
    /// its tools list returns without error. Servers the gateway
    /// reports that are NOT in the generated set — possible
    /// DB-registered extras the manager does not own (§14.6) — are
    /// logged as a warning naming them. Returns the ids that
    /// failed; the caller logs and notes them but does not fail the
    /// step. No state is persisted: the MCP view re-queries the
    /// gateway live (Phase 4).
    pub(crate) async fn mcp_load_check(
        &self,
        ctx: &WizardContext,
        armed_ids: &[String],
        log: &mut Vec<String>,
    ) -> Vec<String> {
        let base = format!(
            "http://{}:{}",
            gateway_probe_host(&ctx.config),
            ctx.config.gateway.port
        );
        let headers = self.gateway_auth_headers(ctx).await;
        let mut registered: Vec<String> = Vec::new();
        match ctx
            .ops
            .http_get_with_headers(&format!("{base}/v1/mcp/server"), &headers)
            .await
        {
            Ok(resp) if resp.status == 200 => {
                registered = crate::parse_mcp_registered_server_ids(&resp.body_str());
            }
            Ok(resp) => log.push(format!(
                "MCP load check: GET /v1/mcp/server returned HTTP {} — cannot confirm which MCP servers the gateway registered",
                resp.status
            )),
            Err(e) => log.push(format!("MCP load check: GET /v1/mcp/server failed: {e}")),
        }
        for extra in registered.iter().filter(|id| !armed_ids.contains(id)) {
            log.push(format!(
                "warning: the gateway reports MCP server '{extra}' which is not in the generated configuration (registered outside the manager, e.g. in the LiteLLM database); it is not managed by this app"
            ));
        }
        let mut failed: Vec<String> = Vec::new();
        for id in armed_ids {
            let mut problems: Vec<String> = Vec::new();
            if !registered.iter().any(|r| r == id) {
                problems.push("not in the gateway's registered MCP server list".to_string());
            }
            let url = format!("{base}/mcp-rest/tools/list?server_id={id}");
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    if let Some(err) = crate::mcp_response_error(&resp.body_str()) {
                        problems.push(format!("tools list returned an error: {err}"));
                    }
                }
                Ok(resp) => {
                    let detail = crate::mcp_response_error(&resp.body_str())
                        .unwrap_or_else(|| format!("HTTP {}", resp.status));
                    problems.push(format!("tools list failed: {detail}"));
                }
                Err(e) => problems.push(format!("tools list request failed: {e}")),
            }
            if problems.is_empty() {
                log.push(format!(
                    "MCP load check passed for '{id}' (registered; tools list OK)"
                ));
            } else {
                log.push(format!(
                    "MCP load check failed for '{id}': {}",
                    problems.join("; ")
                ));
                failed.push(id.clone());
            }
        }
        failed
    }

    pub(crate) async fn wait_healthy(
        &self,
        ctx: &WizardContext,
        pid: u32,
        log: &mut Vec<String>,
    ) -> bool {
        let url = self.gateway_url(ctx);
        let headers = self.gateway_auth_headers(ctx).await;
        let wait_started = std::time::Instant::now();
        push_log(
            ctx,
            log,
            format!(
                "waiting for the gateway to answer on :{} (up to {} probes)",
                ctx.config.gateway.port, self.health_retries
            ),
        );
        for attempt in 1..=self.health_retries {
            // Fail fast when the child is gone: probing a dead process
            // for the full budget reports a timeout and hides the real
            // failure, which the captured log tail then has to explain.
            if !ctx.ops.process_alive(pid) {
                push_log(
                    ctx,
                    log,
                    format!(
                        "gateway process (pid {pid}) exited during startup (probe attempt {attempt})"
                    ),
                );
                return false;
            }
            // Probe outcomes are recorded on the first attempt,
            // every tenth, and on success: a full 120-attempt wait
            // otherwise floods the step log with near-identical
            // lines while showing nothing new.
            let log_attempt = attempt == 1 || attempt.is_multiple_of(10);
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    push_log(
                        ctx,
                        log,
                        format!(
                            "gateway healthy on :{} (attempt {attempt})",
                            ctx.config.gateway.port
                        ),
                    );
                    return true;
                }
                Ok(resp) => {
                    if log_attempt {
                        push_log(
                            ctx,
                            log,
                            format!(
                                "health probe attempt {attempt}: HTTP {} ({:.0}s elapsed)",
                                resp.status,
                                wait_started.elapsed().as_secs_f64()
                            ),
                        );
                    }
                }
                Err(e) => {
                    if log_attempt {
                        push_log(
                            ctx,
                            log,
                            format!(
                                "health probe attempt {attempt}: {e} ({:.0}s elapsed)",
                                wait_started.elapsed().as_secs_f64()
                            ),
                        );
                    }
                }
            }
            if attempt < self.health_retries {
                tokio::time::sleep(std::time::Duration::from_millis(self.health_interval_ms)).await;
            }
        }
        false
    }

    /// Sustained-health confirmation for the DATABASE_URL path:
    /// after the first healthy probe, keep probing for
    /// `db_sustain_ms` and require the gateway to stay alive and
    /// answering the whole window, then close with the standalone
    /// Prisma connect probe. Both known gateway-with-database
    /// defects strike about a second AFTER a plain connect (and a
    /// first health probe) succeeds — the Windows watchdog kill
    /// takes the whole process down (the window catches that), and
    /// the PrismaWrapper connect exception leaves the database dead
    /// while the process keeps serving (only the closing standalone
    /// probe catches that — HTTP health cannot). A zero window
    /// disables the confirmation (tests).
    pub(crate) async fn confirm_sustained_health(
        &self,
        ctx: &WizardContext,
        pid: u32,
        database_url: &str,
        log: &mut Vec<String>,
    ) -> bool {
        if self.db_sustain_ms == 0 {
            return true;
        }
        push_log(
            ctx,
            log,
            format!(
                "confirming the gateway stays healthy for {} s (database path)",
                self.db_sustain_ms / 1000
            ),
        );
        let url = self.gateway_url(ctx);
        let headers = self.gateway_auth_headers(ctx).await;
        let window = std::time::Duration::from_millis(self.db_sustain_ms);
        let deadline = std::time::Instant::now() + window;
        let mut successes = 0u32;
        loop {
            if !ctx.ops.process_alive(pid) {
                log.push(format!(
                    "gateway process (pid {pid}) exited during the database-path confirmation window"
                ));
                return false;
            }
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    successes += 1;
                }
                Ok(resp) => {
                    log.push(format!(
                        "gateway stopped answering during the database-path confirmation window: HTTP {}",
                        resp.status
                    ));
                    return false;
                }
                Err(e) => {
                    log.push(format!(
                        "gateway stopped answering during the database-path confirmation window: {e}"
                    ));
                    return false;
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let step = std::time::Duration::from_millis(self.health_interval_ms.max(1));
            tokio::time::sleep(step.min(remaining)).await;
        }
        // A single lucky 200 at the window's start proves nothing
        // about the span: require at least two successful probes
        // across it.
        if successes < 2 {
            log.push(format!(
                "gateway answered only {successes} health probe(s) during the {} s confirmation window — not a sustained confirmation",
                window.as_secs()
            ));
            return false;
        }
        // Close with the standalone probe: the gateway process can
        // be serving while its database client is dead (the
        // PrismaWrapper connect failure), which no HTTP health
        // probe distinguishes.
        if let Some(detail) = self.prisma_probe_failure(ctx, database_url).await {
            log.push(format!(
                "standalone Prisma probe failed at the end of the confirmation window: {detail} — the gateway is serving but its database client is not connected"
            ));
            return false;
        }
        log.push(format!(
            "gateway still healthy after the {} s database-path confirmation window ({} probes, standalone Prisma probe passed)",
            window.as_secs(),
            successes
        ));
        true
    }
}
