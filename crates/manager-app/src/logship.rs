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

use std::collections::HashMap;
use std::sync::Arc;

use manager_config::LogStreamingLevel;
use manager_logship::shipper::APP_SOURCE;
use manager_logship::{RecordTap, ShipLevel, ShipperConfig, ShipperStatus, TapRecord};
use manager_wizard::SystemOps as _;
use serde_json::Value;

use crate::{AppState, EventSink};

/// Start the shipper if the deployment has opted in. Idempotent:
/// a second call after a successful start is a no-op.
pub async fn start_log_streaming(state: &Arc<AppState>) {
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
        tail_gateway: ls.sources.gateway,
        tail_service_manage: ls.sources.service_manage,
        tail_audit: ls.sources.audit,
        app_events: ls.sources.app,
        tap: Some(state.logship_tap.clone()),
        // S1 is opt-in (D3). The config has no per-backend switch,
        // so the one toggle covers every configured backend.
        backends: ls
            .sources
            .backends
            .then(|| cfg.backends.iter().map(|b| b.id.clone()).collect()),
        literal_secrets: literals,
        spool_cap_bytes: manager_logship::DEFAULT_SPOOL_CAP_BYTES,
    });
    let handle = Arc::new(handle);
    if ls.sources.backends {
        tokio::spawn(pump_backend_rings(Arc::clone(state), Arc::clone(&handle)));
    }
    let _ = state.logship.set(handle);
}

// ---------------------------------------------------------------------------
// S1: per-backend output, process mode
// ---------------------------------------------------------------------------

/// How often the backend rings are followed.
const RING_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// How much of a ring that already holds output ships when the pump
/// first sees it at startup: recent context, not the whole 10k-line
/// ring (the file tails' first-sight rule, in lines).
const RING_FIRST_SIGHT_LINES: usize = 200;

/// Follow the supervisor's per-backend rings and offer new lines to
/// the shipper as `backend:<id>`. Process mode only produces lines
/// here: a service-mode backend's output goes to its log files (which
/// the shipper tails) and its adopted ring stays empty, so no line
/// ships twice. Runs only when `sources.backends` is on, and stops
/// with the shipper.
async fn pump_backend_rings(state: Arc<AppState>, handle: Arc<manager_logship::ShipperHandle>) {
    let mut cursors = HashMap::new();
    let mut first_pass = true;
    while !handle.is_shut_down() {
        // Idle until the shipper has subscribed; cursors do not
        // advance meanwhile, so nothing is skipped.
        if state.logship_tap.is_active() {
            let ids: Vec<String> = state
                .backend_spec_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned()
                .collect();
            let fresh = {
                let sup = state.supervisor().await.lock().await;
                follow_rings(&sup, &ids, &mut cursors, first_pass)
            };
            first_pass = false;
            for record in fresh {
                state.logship_tap.send(record);
            }
        }
        tokio::time::sleep(RING_POLL).await;
    }
}

/// One pass over the rings of `ids`: every line captured since the
/// last pass, as feed records, advancing `cursors`.
fn follow_rings(
    sup: &manager_supervisor::Supervisor,
    ids: &[String],
    cursors: &mut HashMap<String, manager_supervisor::LogCursor>,
    first_pass: bool,
) -> Vec<TapRecord> {
    let mut out = Vec::new();
    for id in ids {
        let (mut records, next) = sup.logs_since(id, cursors.get(id).copied());
        match next {
            Some(cursor) => cursors.insert(id.clone(), cursor),
            None => cursors.remove(id),
        };
        if first_pass && records.len() > RING_FIRST_SIGHT_LINES {
            records.drain(..records.len() - RING_FIRST_SIGHT_LINES);
        }
        let source = manager_logship::shipper::backend_source(id);
        out.extend(records.into_iter().map(|r| {
            TapRecord {
                source: source.clone(),
                stream: match r.stream {
                    manager_supervisor::LogStream::Stdout => "stdout",
                    manager_supervisor::LogStream::Stderr => "stderr",
                }
                .to_string(),
                level: None,
                line: r.line,
            }
        }));
    }
    out
}

// ---------------------------------------------------------------------------
// S5: app / supervisor events
// ---------------------------------------------------------------------------

/// Longest event payload shipped, in characters. Events are small
/// JSON values; the cap only bounds a pathological one.
const MAX_EVENT_PAYLOAD_CHARS: usize = 4096;

/// An [`EventSink`] that offers every event it forwards to the
/// shipper's in-process feed as a source-`app` record. The feed
/// never blocks and costs nothing while no shipper is subscribed,
/// so the wrapped sink behaves exactly as before.
struct TapSink {
    inner: Arc<dyn EventSink>,
    tap: RecordTap,
}

impl EventSink for TapSink {
    fn emit(&self, event: &str, payload: Value) {
        if self.tap.is_active() {
            if let Some(record) = event_record(event, &payload) {
                self.tap.send(record);
            }
        }
        self.inner.emit(event, payload);
    }

    fn toast(&self, title: &str, body: &str) -> Result<(), String> {
        // Toasts are recorded at the `notify` choke point, before
        // the operator's notification switches, not here.
        self.inner.toast(title, body)
    }
}

/// Wrap `inner` so the events it carries also reach the shipper.
/// The registry's `dispatch` wraps the sink of every operation; a
/// shell that hands a sink to a background loop directly (not
/// through `dispatch`) wraps it with this.
pub fn tap_sink(state: &AppState, inner: Arc<dyn EventSink>) -> Arc<dyn EventSink> {
    Arc::new(TapSink {
        inner,
        tap: state.logship_tap.clone(),
    })
}

/// Offer one app-level line (source `app`) to the shipper: state
/// transitions the app observes outside any event — today the
/// notification monitor's (backend died, gateway down, activation
/// resolved), recorded whether or not the toast itself is allowed.
pub(crate) fn offer_app_line(
    state: &AppState,
    level: Option<ShipLevel>,
    line: impl FnOnce() -> String,
) {
    if state.logship_tap.is_active() {
        state.logship_tap.send(TapRecord {
            source: APP_SOURCE.to_string(),
            stream: "stdout".to_string(),
            level,
            line: line(),
        });
    }
}

/// One UI event as a shipped record: `<event> <payload JSON>`.
/// Telemetry samples are gauges on a timer, not log events — they
/// stay out. Download progress ticks ship at debug (below the
/// default floor); everything else takes its level from the outcome
/// fields the payload carries, so a payload key merely *named*
/// `error` (and null) does not make an event an error.
fn event_record(event: &str, payload: &Value) -> Option<TapRecord> {
    use crate::events;
    if event == events::TELEMETRY_UPDATE || event == events::TELEMETRY_TICK {
        return None;
    }
    let level = if event == events::DOWNLOAD_PROGRESS {
        ShipLevel::Debug
    } else {
        let outcome: Vec<&str> = [
            "state", "status", "event", "outcome", "result", "level", "error",
        ]
        .iter()
        .filter_map(|k| payload.get(k).and_then(Value::as_str))
        .collect();
        manager_logship::classify_level(&outcome.join(" "))
    };
    let mut body = payload.to_string();
    if body.chars().count() > MAX_EVENT_PAYLOAD_CHARS {
        body = body.chars().take(MAX_EVENT_PAYLOAD_CHARS).collect();
        body.push('…');
    }
    Some(TapRecord {
        source: APP_SOURCE.to_string(),
        stream: "stdout".to_string(),
        level: Some(level),
        line: format!("{event} {body}"),
    })
}

/// The shipper's status counters, when it is running (surfaced on
/// the Log receiver card's status payload).
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

    #[test]
    fn events_convert_to_app_records() {
        use serde_json::json;
        let rec = event_record(
            crate::events::BACKEND_STATUS,
            &json!({ "id": "planner", "event": "started" }),
        )
        .unwrap();
        assert_eq!(rec.source, "app");
        assert_eq!(rec.level, Some(ShipLevel::Info));
        assert!(rec.line.starts_with("backend-status {"), "{}", rec.line);
        assert!(rec.line.contains(r#""event":"started""#));
        assert!(rec.line.contains(r#""id":"planner""#));

        // A failed step is an error; a null `error` key is not.
        let failed = event_record(
            crate::events::WIZARD_PROGRESS,
            &json!({ "task_id": "t1", "state": "failed", "message": "exit 1" }),
        )
        .unwrap();
        assert_eq!(failed.level, Some(ShipLevel::Error));
        let idle = json!({ "error": null, "phase": "idle" });
        let clean = event_record("app_update_changed", &idle).unwrap();
        assert_eq!(clean.level, Some(ShipLevel::Info));

        // Progress ticks are debug; telemetry samples never ship.
        let tick = event_record(
            crate::events::DOWNLOAD_PROGRESS,
            &json!({ "model_id": "m", "downloaded_bytes": 1 }),
        )
        .unwrap();
        assert_eq!(tick.level, Some(ShipLevel::Debug));
        assert!(event_record(crate::events::TELEMETRY_UPDATE, &json!({ "samples": [] })).is_none());

        // A pathological payload is bounded.
        let blob = json!({ "blob": "a".repeat(3 * MAX_EVENT_PAYLOAD_CHARS) });
        let huge = event_record("x", &blob).unwrap();
        assert!(huge.line.chars().count() <= MAX_EVENT_PAYLOAD_CHARS + 8);
    }

    /// The ring pump's pass: a backend's captured lines come out
    /// once each as `backend:<id>` records with their stream, and a
    /// ring that already holds a lot at first sight ships only its
    /// recent tail.
    #[tokio::test]
    async fn follow_rings_offers_each_backend_line_once() {
        use manager_supervisor::{BackoffPolicy, ProcessSpec, Supervisor};
        let mut sup = Supervisor::new(BackoffPolicy {
            base: std::time::Duration::from_millis(100),
            max: std::time::Duration::from_secs(1),
            max_retries: 0,
        });
        let echo = "for /L %i in (1,1,250) do @echo pumped line %i";
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), echo.to_string()])
        } else {
            let sh = "i=1; while [ $i -le 250 ]; do echo pumped line $i; i=$((i+1)); done";
            ("sh", vec!["-c".to_string(), sh.to_string()])
        };
        sup.register_spec(ProcessSpec {
            id: "planner".into(),
            program: program.into(),
            args,
            env: vec![],
            workdir: None,
            port: None,
            low_priority: false,
        });
        sup.spawn_backend("planner").await.unwrap();
        // Wait for the whole burst to be captured.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while sup
            .logs_query("planner", None, Some("pumped line 250"), 1)
            .total_matching
            == 0
        {
            assert!(
                std::time::Instant::now() < deadline,
                "echo output not captured"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let ids = vec!["planner".to_string(), "never-ran".to_string()];
        let mut cursors = HashMap::new();
        // First pass at startup: only the recent tail of the ring.
        let first = follow_rings(&sup, &ids, &mut cursors, true);
        assert_eq!(first.len(), RING_FIRST_SIGHT_LINES);
        assert!(first
            .iter()
            .all(|r| r.source == "backend:planner" && r.stream == "stdout"));
        assert!(first.last().unwrap().line.contains("pumped line 250"));
        // Nothing new: nothing offered again.
        assert!(follow_rings(&sup, &ids, &mut cursors, false).is_empty());
        let _ = sup.stop("planner", false).await;
    }

    /// The wrapped sink forwards every event unchanged, and offers
    /// it to the feed only while a shipper is subscribed.
    #[test]
    fn tap_sink_forwards_and_offers() {
        use serde_json::json;
        struct Recording(std::sync::Mutex<Vec<(String, Value)>>);
        impl EventSink for Recording {
            fn emit(&self, event: &str, payload: Value) {
                self.0.lock().unwrap().push((event.to_string(), payload));
            }
        }
        let inner = Arc::new(Recording(Default::default()));
        let tap = RecordTap::new();
        let sink = TapSink {
            inner: inner.clone(),
            tap: tap.clone(),
        };
        // No subscriber: forwarded, nothing offered.
        sink.emit("backend-status", json!({ "id": "a", "event": "started" }));
        let mut rx = tap.subscribe();
        sink.emit("backend-status", json!({ "id": "a", "event": "stopped" }));
        sink.emit(crate::events::TELEMETRY_UPDATE, json!({ "samples": [] }));
        assert_eq!(inner.0.lock().unwrap().len(), 3);
        let offered = rx.try_recv().unwrap();
        assert!(offered.line.contains("stopped"));
        // Telemetry and pre-subscribe events were not offered.
        assert!(rx.try_recv().is_err());
    }
}
