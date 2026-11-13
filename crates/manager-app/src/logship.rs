//! Log-streaming startup, in-process sources and status
//! (log-streaming design §6.1, §6.5; docs/log-streaming.md).
//!
//! The shipper (crate `manager-logship`) starts after config load
//! when `log_streaming.enabled` is set and the destination endpoint
//! passes the endpoint policy, and is restarted whenever its saved
//! settings change (`apply_log_streaming`). Enablement is the
//! config-file section plus the Credential Manager token (D6). This
//! module also feeds the shipper the sources that have no file to
//! tail — app events and process-mode backend output — and builds
//! the per-source status the Log receiver card shows. Every failure
//! to start is written to the local `shipper.log`, surfaced on that
//! card, and never fails the app: a deployment that never opts in
//! behaves exactly as before.

use std::collections::HashMap;
use std::sync::Arc;

use manager_config::LogStreamingLevel;
use manager_logship::shipper::APP_SOURCE;
use manager_logship::{RecordTap, ShipLevel, ShipperConfig, ShipperStatus, TapRecord};
use manager_wizard::SystemOps as _;
use serde_json::Value;

use crate::{AppState, EventSink};

/// The running shipper and what it was started with (held in
/// `AppState` behind a std Mutex: every access is a short,
/// synchronous read or swap).
#[derive(Default)]
pub(crate) struct LogshipSlot {
    handle: Option<Arc<manager_logship::ShipperHandle>>,
    /// The settings the running shipper was started with
    /// ([`applied_settings`]); `apply_log_streaming` is a no-op while
    /// the saved config still matches.
    applied: Option<Value>,
    /// Why streaming is enabled but no shipper is running (refused
    /// endpoint, missing token, …). Shown on the Log receiver card.
    note: Option<String>,
}

/// How long a settings change waits for the previous shipper to
/// stop: longer than its HTTP timeout (15 s), so an in-flight POST
/// can finish or fail.
const SHIPPER_STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Everything the shipper is configured from, as one comparable
/// value: the `log_streaming` section, plus the backend ids when
/// per-backend logs are on (their file tails are per id).
fn applied_settings(cfg: &manager_config::AppConfig) -> Value {
    let backends: Vec<&str> = if cfg.log_streaming.sources.backends {
        cfg.backends.iter().map(|b| b.id.as_str()).collect()
    } else {
        Vec::new()
    };
    serde_json::json!({
        "log_streaming": serde_json::to_value(&cfg.log_streaming).unwrap_or(Value::Null),
        "backends": backends,
    })
}

/// Start the shipper at app start if the deployment has opted in.
pub async fn start_log_streaming(state: &Arc<AppState>) {
    apply_log_streaming(state).await;
}

/// Bring the shipper in line with the saved config: start it, stop
/// it, or restart it with new settings. Called at app start and
/// after every config save, so a saved `log_streaming` change takes
/// effect without restarting LLMdil. A no-op when a shipper is
/// already running with the saved settings. Never fails the caller:
/// a shipper that cannot start leaves a note for the status surface
/// and a line in `shipper.log`.
///
/// Restarting loses nothing: the old shipper parks what it holds in
/// the spool as it stops, tail offsets persist, and the new shipper
/// starts only after the old one has ended (two shippers on one
/// spool would reuse sequence numbers).
pub async fn apply_log_streaming(state: &Arc<AppState>) {
    let _one_at_a_time = state.logship_apply.lock().await;
    let cfg = state.config.read().await.clone();
    let wanted = applied_settings(&cfg);
    let previous = {
        let mut slot = state.logship.lock().unwrap_or_else(|e| e.into_inner());
        if slot.handle.is_some() && slot.applied.as_ref() == Some(&wanted) {
            return;
        }
        slot.applied = None;
        slot.handle.take()
    };
    let log = |line: &str| manager_logship::shipper::append_shipper_log(&state.data_dir, line);
    if let Some(old) = previous {
        if !old.shutdown_and_wait(SHIPPER_STOP_LIMIT).await {
            let note = "the previous shipper did not stop in time; restart LLMdil to apply \
                        the log streaming settings";
            log(note);
            set_note(state, Some(note.to_string()));
            return;
        }
        log("shipper stopped to apply changed log streaming settings");
    }
    match spawn_shipper(state, &cfg).await {
        Ok(handle) => {
            let mut slot = state.logship.lock().unwrap_or_else(|e| e.into_inner());
            slot.applied = handle.is_some().then_some(wanted);
            slot.handle = handle;
            slot.note = None;
        }
        Err(reason) => {
            log(&format!(
                "log streaming is enabled but the shipper was not started: {reason}"
            ));
            set_note(state, Some(reason));
        }
    }
}

fn set_note(state: &AppState, note: Option<String>) {
    state.logship.lock().unwrap_or_else(|e| e.into_inner()).note = note;
}

/// Spawn a shipper for `cfg`. `Ok(None)` when streaming is off;
/// `Err` is the operator-facing reason it is on but cannot start.
async fn spawn_shipper(
    state: &Arc<AppState>,
    cfg: &manager_config::AppConfig,
) -> Result<Option<Arc<manager_logship::ShipperHandle>>, String> {
    let ls = &cfg.log_streaming;
    if !ls.enabled {
        return Ok(None);
    }
    // SEC-16: plain http:// only to a loopback endpoint.
    manager_logship::check_endpoint(&ls.endpoint)?;
    let ops = manager_wizard::ops::RealSystemOps::default();
    let Some(token) = ops
        .credential_read(manager_config::LOG_STREAMING_TOKEN_CRED_TARGET)
        .await
    else {
        return Err(
            "the ingest token credential (local-llm-service-manager/log-streaming-token) \
             is missing"
                .to_string(),
        );
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
    Ok(Some(handle))
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
    } else if event == events::SERVICE_LIFECYCLE
        && matches!(
            payload.get("event").and_then(Value::as_str),
            Some("crashed" | "failed")
        )
    {
        // A lifecycle failure is an error whatever state it settled
        // in (a crash can settle as `stopped` with intent Running).
        ShipLevel::Error
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
    let slot = state.logship.lock().unwrap_or_else(|e| e.into_inner());
    slot.handle.as_ref().map(|h| h.status())
}

/// Why streaming is enabled but no shipper is running, if so.
pub fn log_streaming_note(state: &AppState) -> Option<String> {
    let slot = state.logship.lock().unwrap_or_else(|e| e.into_inner());
    slot.note.clone()
}

/// The `shipper` object of the `log_receiver_status` payload: what
/// the shipper is doing, source by source, so the Settings card
/// never implies a source ships when it does not.
///
/// Running: the shipper's own rows (`active` / `disabled` /
/// `file_missing` / `waiting` / `unavailable` / `local_only`, with
/// record counts) and counters. Not running (streaming off, or on
/// but unable to start — `note` says why): every source is listed
/// from the config as `disabled` or `not_running`. `configured`
/// carries the saved switches for the card's editor. Saved changes
/// apply as they are saved (`apply_log_streaming`), so
/// `restart_pending` is normally false: it reports a saved toggle
/// that still differs from what the running shipper started with.
pub fn shipper_surface(
    cfg: &manager_config::LogStreamingConfig,
    status: Option<&ShipperStatus>,
    note: Option<&str>,
) -> Value {
    use manager_logship::shipper::BACKEND_SOURCE_PREFIX;
    use manager_logship::SourceState;
    let s = &cfg.sources;
    let configured = serde_json::json!({
        "enabled": cfg.enabled,
        "sources": {
            "wizard": s.wizard,
            "optimize": s.optimize,
            "gateway": s.gateway,
            "service_manage": s.service_manage,
            "audit": s.audit,
            "app": s.app,
            "backends": s.backends,
        },
    });
    // The config toggle behind a status row's source name.
    let toggle = |source: &str| match source {
        "wizard" => s.wizard,
        "optimize" => s.optimize,
        "gateway" => s.gateway,
        "service-manage" => s.service_manage,
        "audit" => s.audit,
        APP_SOURCE => s.app,
        other => (other == "backends" || other.starts_with(BACKEND_SOURCE_PREFIX)) && s.backends,
    };
    let Some(status) = status else {
        let sources: Vec<Value> = [
            "wizard",
            "optimize",
            "gateway",
            "service-manage",
            "audit",
            APP_SOURCE,
            "backends",
        ]
        .iter()
        .map(|source| {
            serde_json::json!({
                "source": source,
                "state": if toggle(source) { "not_running" } else { "disabled" },
                "records": 0,
            })
        })
        .collect();
        return serde_json::json!({
            "running": false,
            "state": "not_running",
            "restart_pending": false,
            "note": note,
            "configured": configured,
            "sources": sources,
        });
    };
    let restart_pending = !cfg.enabled
        || status
            .sources
            .iter()
            .any(|row| (row.state != SourceState::Disabled) != toggle(&row.source));
    serde_json::json!({
        "running": true,
        "state": status.state,
        "restart_pending": restart_pending,
        "note": note,
        "configured": configured,
        "queued": status.queued,
        "spooled_bytes": status.spooled_bytes,
        "delivered": status.delivered,
        "dropped_queue": status.dropped_queue,
        "dropped_spool": status.dropped_spool,
        "dropped_events": status.dropped_events,
        "last_delivered": status.last_delivered,
        "sources": status.sources,
    })
}

/// Shut the shipper down (app shutdown path).
pub fn stop_log_streaming(state: &AppState) {
    let slot = state.logship.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = slot.handle.as_ref() {
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

        // Lifecycle events of every service ship as app records; a
        // crash is an error even when it settled as `stopped`.
        let restarting = event_record(
            crate::events::SERVICE_LIFECYCLE,
            &crate::lifecycle::event_payload(
                "gateway",
                "restarting",
                crate::lifecycle::LifecycleState::Restarting,
                Some(crate::lifecycle::Origin::Activation),
                None,
            ),
        )
        .unwrap();
        assert_eq!(restarting.source, "app");
        assert_eq!(restarting.level, Some(ShipLevel::Info));
        assert!(restarting.line.starts_with("service-lifecycle {"), "{}", restarting.line);
        let crashed = event_record(
            crate::events::SERVICE_LIFECYCLE,
            &json!({ "key": "backend:a", "event": "crashed", "state": "stopped" }),
        )
        .unwrap();
        assert_eq!(crashed.level, Some(ShipLevel::Error));

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

    #[test]
    fn shipper_surface_reports_every_source_honestly() {
        use manager_logship::{SourceState, SourceStatus};
        let mut cfg = manager_config::LogStreamingConfig::default();
        let state_of = |v: &Value, source: &str| {
            v["sources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["source"] == source)
                .unwrap_or_else(|| panic!("no row for {source}"))["state"]
                .clone()
        };

        // Not running, streaming off: nothing ships, nothing pending.
        let off = shipper_surface(&cfg, None, None);
        assert_eq!(off["running"], false);
        assert_eq!(off["restart_pending"], false);
        assert_eq!(off["note"], Value::Null);
        for source in ["wizard", "optimize", "gateway", "audit", "app"] {
            assert_eq!(state_of(&off, source), "not_running");
        }
        assert_eq!(state_of(&off, "service-manage"), "not_running");
        assert_eq!(state_of(&off, "backends"), "disabled");
        // The saved switches ride along for the card's editor.
        assert_eq!(off["configured"]["enabled"], false);
        assert_eq!(off["configured"]["sources"]["service_manage"], true);
        assert_eq!(off["configured"]["sources"]["backends"], false);

        // Enabled in the config but unable to start: the reason is
        // carried, and nothing claims to ship.
        cfg.enabled = true;
        let blocked = shipper_surface(&cfg, None, Some("no endpoint is configured"));
        assert_eq!(blocked["running"], false);
        assert_eq!(blocked["note"], "no endpoint is configured");
        assert_eq!(blocked["configured"]["enabled"], true);
        assert_eq!(state_of(&blocked, "gateway"), "not_running");

        // Running: the shipper's own rows, verbatim.
        let row = |source: &str, state| SourceStatus {
            source: source.to_string(),
            state,
            records: 3,
        };
        let status = ShipperStatus {
            state: "connected".to_string(),
            dropped_events: 2,
            sources: vec![
                row("wizard", SourceState::Active),
                row("gateway", SourceState::FileMissing),
                row("audit", SourceState::Active),
                row("app", SourceState::Active),
                row("optimize", SourceState::Active),
                row("service-manage", SourceState::Active),
                row("backends", SourceState::Disabled),
            ],
            ..ShipperStatus::default()
        };
        let on = shipper_surface(&cfg, Some(&status), None);
        assert_eq!(on["running"], true);
        assert_eq!(on["state"], "connected");
        assert_eq!(on["dropped_events"], 2);
        assert_eq!(state_of(&on, "gateway"), "file_missing");
        assert_eq!(state_of(&on, "backends"), "disabled");
        assert_eq!(on["sources"][0]["records"], 3);
        assert_eq!(on["restart_pending"], false);

        // A toggle saved since the shipper started is flagged — in
        // both directions.
        let pending = |cfg: &manager_config::LogStreamingConfig| {
            shipper_surface(cfg, Some(&status), None)["restart_pending"].clone()
        };
        cfg.sources.audit = false;
        assert_eq!(pending(&cfg), true);
        cfg.sources.audit = true;
        cfg.sources.backends = true;
        assert_eq!(pending(&cfg), true);
        cfg.sources.backends = false;

        // A source held on the machine by policy is on, not pending.
        let held = ShipperStatus {
            sources: vec![row("audit", SourceState::LocalOnly)],
            ..ShipperStatus::default()
        };
        let v = shipper_surface(&cfg, Some(&held), None);
        assert_eq!(state_of(&v, "audit"), "local_only");
        assert_eq!(v["restart_pending"], false);
    }

    fn apply_test_state(name: &str) -> (Arc<AppState>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("llmdil-logship-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = manager_config::AppConfig::default_config();
        cfg.data_dir = dir.clone();
        let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
        (state, dir)
    }

    /// Saving `log_streaming` settings applies them at once — no app
    /// restart. SEC-16: enabling streaming toward a non-loopback
    /// `http://` endpoint is saved, but the shipper is refused and
    /// the status carries the reason; turning streaming off again
    /// clears it. (The refusal comes before the token is read, so
    /// this never touches the credential store.)
    #[tokio::test]
    async fn saved_settings_apply_live_and_a_plain_http_endpoint_is_refused() {
        let (state, dir) = apply_test_state("sec16");
        let sink: Arc<dyn EventSink> = Arc::new(crate::NoopSink);
        let surface = |state: &Arc<AppState>, cfg: &manager_config::AppConfig| {
            shipper_surface(
                &cfg.log_streaming,
                log_streaming_status(state).as_ref(),
                log_streaming_note(state).as_deref(),
            )
        };

        // Off: nothing running, no note.
        apply_log_streaming(&state).await;
        assert!(log_streaming_status(&state).is_none());
        assert!(log_streaming_note(&state).is_none());

        let patch = serde_json::json!({ "log_streaming": {
            "enabled": true,
            "endpoint": "http://192.0.2.10:8473",
        }});
        crate::op_update_config(&state, &sink, patch).await.unwrap();
        assert!(
            log_streaming_status(&state).is_none(),
            "refused: not running"
        );
        let note = log_streaming_note(&state).expect("the refusal is surfaced");
        assert!(note.contains("loopback"), "{note}");
        let cfg = state.config.read().await.clone();
        let v = surface(&state, &cfg);
        assert_eq!(v["running"], false);
        assert_eq!(v["configured"]["enabled"], true);
        assert!(v["note"].as_str().unwrap().contains("https://"));
        let log = std::fs::read_to_string(dir.join("logs").join("shipper.log")).unwrap();
        assert!(log.contains("shipper was not started"), "{log}");

        // A source switch saved while it cannot run is still saved.
        let patch = serde_json::json!({ "log_streaming": { "sources": { "gateway": false } } });
        crate::op_update_config(&state, &sink, patch).await.unwrap();
        let cfg = state.config.read().await.clone();
        assert_eq!(
            surface(&state, &cfg)["configured"]["sources"]["gateway"],
            false
        );

        // Off again: the note goes with it.
        let patch = serde_json::json!({ "log_streaming": { "enabled": false } });
        crate::op_update_config(&state, &sink, patch).await.unwrap();
        assert!(log_streaming_note(&state).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn applied_settings_change_with_what_the_shipper_is_built_from() {
        let mut cfg = manager_config::AppConfig::default_config();
        let base = applied_settings(&cfg);
        // Unrelated config does not restart the shipper.
        cfg.winml.enabled = !cfg.winml.enabled;
        assert_eq!(applied_settings(&cfg), base);
        // A source switch, the endpoint, or the floor does.
        cfg.log_streaming.sources.audit = false;
        assert_ne!(applied_settings(&cfg), base);
        cfg.log_streaming.sources.audit = true;
        cfg.log_streaming.endpoint = "http://127.0.0.1:8473".to_string();
        assert_ne!(applied_settings(&cfg), base);
        // Backend ids matter only while per-backend logs are on.
        let mut with_backends = manager_config::AppConfig::default_config();
        with_backends.log_streaming.sources.backends = true;
        let on = applied_settings(&with_backends);
        with_backends.backends.pop();
        assert_ne!(applied_settings(&with_backends), on);
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
