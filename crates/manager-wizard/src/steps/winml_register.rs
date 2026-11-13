//! Step 9: register the Windows ML backend.

use super::*;

// ---------------------------------------------------------------------------
// 9. Register Windows ML backend
// ---------------------------------------------------------------------------

/// Step 9: probe WinMLServer, validate the model class, record the
/// registration with the researched limits verbatim.
pub struct WinMlRegister;

impl WinMlRegister {
    fn registration_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("winml").join("registration.json")
    }

    fn health_url(&self, ctx: &WizardContext) -> String {
        format!("http://127.0.0.1:{}/v1/models", ctx.config.winml.port)
    }
}

#[async_trait]
impl WizardStep for WinMlRegister {
    fn id(&self) -> &str {
        STEP_WINML_REGISTER
    }
    fn name(&self) -> &str {
        "Register Windows ML backend"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // The probe is an HTTP GET, surfaced here for transparency.
        vec![PlannedCommand::new(vec![
            "http-get".to_string(),
            self.health_url(ctx),
        ])]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("start WinMLServer, then re-run winml-register")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        if !ctx.config.winml.enabled {
            return Ok(StepOutcome::skipped(
                "Windows ML backend disabled in config",
            ));
        }
        let model_id = match &ctx.config.winml.model_id {
            Some(m) => m.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "winml.model_id not configured",
                    vec!["set winml.model_id to a tool-runner-class model".to_string()],
                ))
            }
        };
        // Authoritative eligibility check lives in manager-winml.
        if let Err(e) = manager_winml::model_class_eligible(&model_id) {
            return Ok(StepOutcome::failed(
                format!("model '{model_id}' ineligible for Windows ML: {e}"),
                vec![format!("{e:?}")],
            ));
        }
        let mut log = vec![format!("model '{model_id}' eligible (tool-runner class)")];
        match ctx.ops.http_get(&self.health_url(ctx)).await {
            Ok(resp) if resp.status == 200 => {
                log.push(format!(
                    "WinMLServer responding on :{}",
                    ctx.config.winml.port
                ));
            }
            Ok(resp) => {
                return Ok(StepOutcome::failed(
                    format!("WinMLServer unhealthy: HTTP {}", resp.status),
                    log,
                ))
            }
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!(
                        "WinMLServer not reachable on :{}: {e}",
                        ctx.config.winml.port
                    ),
                    log,
                ))
            }
        }
        // Record the registration with the researched limits verbatim.
        let limits = manager_winml::WinMlLimits::known_limits();
        let reg = serde_json::json!({
            "model_id": model_id,
            "port": ctx.config.winml.port,
            "endpoint": self.health_url(ctx),
            "limits": limits,
            "registered_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = self.registration_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(&path, json_pretty(&reg).as_bytes())
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("wrote {}", path.display()));
        for stmt in manager_winml::WinMlLimits::statements() {
            log.push(format!("limit: {stmt}"));
        }
        Ok(StepOutcome::done(
            format!(
                "Windows ML backend registered: {model_id} on :{} (experimental, no tensor-split)",
                ctx.config.winml.port
            ),
            log,
        ))
    }
}
