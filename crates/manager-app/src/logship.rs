//! Log-streaming startup and status (log-streaming design §6.1,
//! §6.5, §8 MVP).
//!
//! The shipper (crate `manager-logship`) starts once, after config
//! load, when `log_streaming.enabled` is set and a destination
//! endpoint is configured. Enablement in the MVP is the config-file
//! section plus the Credential Manager token (D6) — the Settings
//! card and registry ops are Phase 2. Every failure to start is
//! written to the local `shipper.log` and never fails the app: a
//! deployment that never opts in behaves exactly as before.

use manager_config::LogStreamingLevel;
use manager_logship::{ShipLevel, ShipperConfig, ShipperStatus};
use manager_wizard::SystemOps as _;

use crate::AppState;

/// Start the shipper if the deployment has opted in. Idempotent:
/// a second call after a successful start is a no-op.
pub async fn start_log_streaming(state: &AppState) {
    if state.logship.get().is_some() {
        return;
    }
    let cfg = state.config.read().await.clone();
    let ls = &cfg.log_streaming;
    if !ls.enabled {
        return;
    }
    let log = |line: &str| manager_logship::shipper::append_shipper_log(&state.data_dir, line);
    if ls.endpoint.trim().is_empty() {
        log("log streaming is enabled but no endpoint is configured; shipper not started");
        return;
    }
    let ops = manager_wizard::ops::RealSystemOps::default();
    let Some(token) = ops
        .credential_read(manager_config::LOG_STREAMING_TOKEN_CRED_TARGET)
        .await
    else {
        log(
            "log streaming is enabled but the ingest token credential \
             (local-llm-service-manager/log-streaming-token) is missing; shipper not started",
        );
        return;
    };

    // Redaction layer 1 (design §5.2): the deployment's known
    // secret values — the token itself, the LiteLLM master key, the
    // Postgres password, the OpenRouter key when configured, and
    // every MCP server's stored credential value. Values are read
    // the same way the gateway path reads them and are handed to
    // the redactor in memory only.
    let mut literals: Vec<String> = vec![token.clone()];
    let mut targets: Vec<&str> = vec![
        manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET,
        manager_wizard::POSTGRES_PASSWORD_CRED_TARGET,
    ];
    if let Some(r) = cfg.gateway.openrouter.cred_ref.as_deref().filter(|r| !r.is_empty()) {
        targets.push(r);
    }
    for target in targets {
        if let Some(v) = ops.credential_read(target).await {
            expand_literal(&v, &mut literals);
        }
    }
    for server in &cfg.mcp.servers {
        if let Some(r) = server.cred_ref.as_deref().filter(|r| !r.is_empty()) {
            if let Some(v) = ops.credential_read(r).await {
                expand_literal(&v, &mut literals);
            }
        }
    }

    let machine = ls
        .machine_name
        .clone()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(manager_logship::default_machine_name);
    let handle = manager_logship::spawn(ShipperConfig {
        data_dir: state.data_dir.clone(),
        endpoint: ls.endpoint.clone(),
        token,
        machine,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        level_floor: match ls.level_floor {
            LogStreamingLevel::Error => ShipLevel::Error,
            LogStreamingLevel::Warn => ShipLevel::Warn,
            LogStreamingLevel::Info => ShipLevel::Info,
            LogStreamingLevel::Debug => ShipLevel::Debug,
        },
        tail_wizard: ls.sources.wizard,
        tail_optimize: ls.sources.optimize,
        literal_secrets: literals,
        spool_cap_bytes: manager_logship::DEFAULT_SPOOL_CAP_BYTES,
    });
    let _ = state.logship.set(std::sync::Arc::new(handle));
}

/// The shipper's status counters, when it is running (Phase 2's
/// `log_streaming_status` op reads these).
pub fn log_streaming_status(state: &AppState) -> Option<ShipperStatus> {
    state.logship.get().map(|h| h.status())
}

/// Shut the shipper down (app shutdown path).
pub fn stop_log_streaming(state: &AppState) {
    if let Some(h) = state.logship.get() {
        h.shutdown();
    }
}

/// One credential value as literal blocklist entries: the raw value,
/// plus every string leaf when the value is a JSON document (MCP
/// credential stores hold env maps) — a secret echoed out of its
/// JSON envelope must still be caught.
fn expand_literal(value: &str, out: &mut Vec<String>) {
    out.push(value.to_string());
    if let Ok(doc) = serde_json::from_str::<serde_json::Value>(value) {
        collect_strings(&doc, out);
    }
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_literal_collects_json_leaves() {
        let mut out = Vec::new();
        expand_literal(r#"{"A":"v-one","B":{"C":"v-two"}}"#, &mut out);
        assert!(out.iter().any(|s| s == "v-one"));
        assert!(out.iter().any(|s| s == "v-two"));
        let mut plain = Vec::new();
        expand_literal("plain-secret", &mut plain);
        assert_eq!(plain, vec!["plain-secret".to_string()]);
    }
}
