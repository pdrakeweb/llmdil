//! Activation apply tests (service-lifecycle design WP7): the pure
//! per-item bookkeeping (same outcome/detail strings as the desktop
//! funnel it replaces), the apply lock, the `activation_applying`
//! payloads and the monitor's suppression while an apply runs.

use super::*;
use crate::{AppConfig, NoopSink};
use std::path::PathBuf;
use std::sync::Mutex;

fn change(scope: &str, targets: &[&str], activation: &str) -> PlanChange {
    PlanChange {
        ts: "2026-11-12T10:00:00Z".to_string(),
        kind: if scope == "backend" {
            "backend_flags"
        } else {
            "gateway_config"
        }
        .to_string(),
        summary: format!("{scope} change"),
        scope: scope.to_string(),
        targets: targets.iter().map(|t| t.to_string()).collect(),
        activation: activation.to_string(),
    }
}

fn set(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// a, b restart; c is stopped (next start); one gateway change.
fn sample_changes() -> Vec<PlanChange> {
    vec![
        change("backend", &["a"], "restart"),
        change("backend", &["b"], "restart"),
        change("backend", &["c"], "next_start"),
        change("gateway", &[], "gateway_restart"),
    ]
}

#[test]
fn items_start_as_unattempted_skips() {
    let items = plan_items(&sample_changes());
    assert_eq!(items.len(), 4);
    for i in &items {
        assert_eq!(i.outcome, "skipped");
        assert_eq!(i.detail.as_deref(), Some(NOT_ATTEMPTED));
    }
    assert_eq!(items[0].targets, vec!["a".to_string()]);
    assert_eq!(items[3].activation, "gateway_restart");
}

#[test]
fn all_ok_applies_every_restart_item() {
    let mut items = plan_items(&sample_changes());
    mark_backends_restarted(&mut items, &set(&["a", "b"]));
    mark_gateway_result(&mut items, true, "");
    assert_eq!(items[0].outcome, "applied");
    assert_eq!(items[0].detail, None);
    assert_eq!(items[1].outcome, "applied");
    // The stopped backend's change was already effective.
    assert_eq!(items[2].outcome, "skipped");
    assert_eq!(items[2].detail.as_deref(), Some(ALREADY_EFFECTIVE));
    assert_eq!(items[3].outcome, "applied");
    assert_eq!(items[3].detail, None);
    assert_eq!(completed_status(&items), "completed");
}

#[test]
fn backend_failure_mid_list_stops_the_apply() {
    let mut items = plan_items(&sample_changes());
    let restart_ids = vec!["a".to_string(), "b".to_string()];
    // a restarted, b failed.
    mark_backend_failure(
        &mut items,
        "b",
        "did not become healthy",
        &restart_ids,
        &set(&["a"]),
        true,
    );
    assert_eq!(items[0].outcome, "applied");
    assert_eq!(items[0].detail, None);
    assert_eq!(items[1].outcome, "failed");
    assert_eq!(items[1].detail.as_deref(), Some("did not become healthy"));
    assert_eq!(items[2].outcome, "skipped");
    assert_eq!(items[2].detail.as_deref(), Some(ALREADY_EFFECTIVE));
    assert_eq!(items[3].outcome, "skipped");
    assert_eq!(
        items[3].detail.as_deref(),
        Some("Not attempted — the apply stopped before the gateway restart")
    );

    // a failed first: b's item waits on a restart the apply never ran.
    let mut items = plan_items(&sample_changes());
    mark_backend_failure(
        &mut items,
        "a",
        "boom",
        &restart_ids,
        &HashSet::new(),
        false,
    );
    assert_eq!(items[0].outcome, "failed");
    assert_eq!(items[1].outcome, "skipped");
    assert_eq!(
        items[1].detail.as_deref(),
        Some("Not attempted — the apply stopped after a failed backend restart")
    );
    // No gateway change pending: the gateway item keeps its original detail.
    assert_eq!(items[3].detail.as_deref(), Some(NOT_ATTEMPTED));
}

#[test]
fn gateway_failure_completes_with_failures() {
    let mut items = plan_items(&sample_changes());
    mark_backends_restarted(&mut items, &set(&["a", "b"]));
    mark_gateway_result(
        &mut items,
        false,
        "Start-gateway step failed — prisma migrate failed",
    );
    assert_eq!(items[0].outcome, "applied");
    assert_eq!(items[3].outcome, "failed");
    assert_eq!(
        items[3].detail.as_deref(),
        Some("Start-gateway step failed — prisma migrate failed")
    );
    assert_eq!(completed_status(&items), "completed_with_failures");

    let mut items = plan_items(&[change("gateway", &[], "gateway_restart")]);
    mark_gateway_result(&mut items, false, "");
    assert_eq!(
        items[0].detail.as_deref(),
        Some("The gateway restart did not complete")
    );
}

#[test]
fn modes_and_sources_normalize() {
    assert_eq!(parse_mode(None).unwrap().0, ApplyMode::Now);
    assert_eq!(
        parse_mode(Some("Quiesce")).unwrap(),
        (ApplyMode::Drain, "quiesce".to_string())
    );
    assert_eq!(parse_mode(Some("drain")).unwrap().0, ApplyMode::Drain);
    for m in ["now", "restart", "start"] {
        assert_eq!(
            parse_mode(Some(m)).unwrap(),
            (ApplyMode::Now, m.to_string())
        );
    }
    assert!(matches!(
        parse_mode(Some("later")),
        Err(ApiError::InvalidInput(_))
    ));
    assert_eq!(parse_source(None).unwrap(), "desktop");
    assert_eq!(parse_source(Some("API")).unwrap(), "api");
    assert_eq!(parse_source(Some("cli")).unwrap(), "cli");
    assert!(matches!(
        parse_source(Some("evil")),
        Err(ApiError::InvalidInput(_))
    ));
}

fn apply_state(name: &str) -> (Arc<AppState>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-act-apply-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = AppConfig::default_config();
    cfg.data_dir = dir.clone();
    cfg.gateway.port = 40098;
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    (state, dir)
}

fn sample_progress() -> ApplyProgress {
    ApplyProgress {
        source: "api".to_string(),
        mode: "now".to_string(),
        started_at: chrono::Utc::now(),
        step: "backend".to_string(),
        target: Some("planner".to_string()),
        message: "Restarting backend planner…".to_string(),
    }
}

fn set_progress(state: &AppState, p: Option<ApplyProgress>) {
    *state.activation_progress.lock().unwrap() = p;
}

#[tokio::test]
async fn a_second_apply_while_one_runs_is_busy() {
    let (state, dir) = apply_state("busy");
    let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
    let held = state.activation_apply_lock.lock().await;
    set_progress(&state, Some(sample_progress()));
    let err = op_activation_apply(&state, &sink, None, Some("desktop".to_string()))
        .await
        .unwrap_err();
    match err {
        ApiError::Busy(msg) => {
            assert!(msg.contains("started by api"), "{msg}");
            assert!(msg.contains("Restarting backend planner"), "{msg}");
        }
        other => panic!("expected Busy, got {other}"),
    }
    // The refusal left the running apply's progress alone.
    assert!(current_progress(&state).is_some());
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_apply_with_nothing_pending_runs_nothing_and_records_nothing() {
    let (state, dir) = apply_state("idle");
    let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
    let v = op_activation_apply(
        &state,
        &sink,
        Some("drain".to_string()),
        Some("cli".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "nothing_pending");
    assert_eq!(v["gateway_restarted"], false);
    assert!(activation_history::load_history(&dir).executions.is_empty());
    // Progress cleared, lock released.
    assert!(current_progress(&state).is_none());
    assert!(state.activation_apply_lock.try_lock().is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plan_and_status_show_the_apply_in_progress() {
    let (state, dir) = apply_state("payload");
    let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
    // A gateway-scope change with the gateway down: already effective
    // at the next start, so not pending while idle ...
    let cfg = state.config.read().await.clone();
    crate::activation::record_change(
        &state.data_dir,
        &cfg,
        &cfg,
        "gateway_config",
        "test gateway change".to_string(),
        "gateway",
        Vec::new(),
        &std::collections::HashMap::new(),
        &HashSet::new(),
        &HashSet::new(),
    )
    .unwrap();
    let plan = crate::op_gateway_activation_plan(&state, &sink)
        .await
        .unwrap();
    assert!(plan["activation_applying"].is_null());
    assert_eq!(plan["pending"], false);
    let st = crate::op_gateway_status(&state, &sink).await.unwrap();
    assert!(st["activation_applying"].is_null());
    assert_eq!(st["activation_pending"], false);

    // ... while an apply runs, both payloads carry its progress and
    // keep the plan pending (the banner and card never vanish).
    set_progress(&state, Some(sample_progress()));
    let plan = crate::op_gateway_activation_plan(&state, &sink)
        .await
        .unwrap();
    assert_eq!(plan["activation_applying"]["step"], "backend");
    assert_eq!(plan["activation_applying"]["target"], "planner");
    assert_eq!(plan["activation_applying"]["source"], "api");
    assert_eq!(plan["pending"], true);
    assert_eq!(plan["gateway_pending"], true);
    let st = crate::op_gateway_status(&state, &sink).await.unwrap();
    assert_eq!(
        st["activation_applying"]["message"],
        "Restarting backend planner…"
    );
    assert_eq!(st["activation_pending"], true);
    set_progress(&state, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Records every toast.
#[derive(Default)]
struct ToastSink(Mutex<Vec<String>>);
impl EventSink for ToastSink {
    fn emit(&self, _event: &str, _payload: Value) {}
    fn toast(&self, title: &str, _body: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(title.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn monitor_does_not_fire_applied_while_applying() {
    let (state, dir) = apply_state("monitor");
    let sink = ToastSink::default();
    // The monitor last saw a pending plan; nothing is pending now.
    state.notify_watch.lock().unwrap().activation_pending = Some(true);
    set_progress(&state, Some(sample_progress()));
    assert!(notify::activation_resolution_suppressed(&state));
    notify::monitor_tick(&state, &sink).await;
    assert!(
        !sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|t| t == "Activation plan applied"),
        "{:?}",
        sink.0.lock().unwrap()
    );
    // The watch is untouched for the apply's own completion to settle.
    assert_eq!(
        state.notify_watch.lock().unwrap().activation_pending,
        Some(true)
    );

    // Once the apply is over (and did not fire it itself) the monitor
    // reports the resolution as before.
    set_progress(&state, None);
    notify::monitor_tick(&state, &sink).await;
    assert!(sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|t| t == "Activation plan applied"));
    assert_eq!(
        state.notify_watch.lock().unwrap().activation_pending,
        Some(false)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
