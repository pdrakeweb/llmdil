//! Gateway / PostgreSQL lifecycle op tests (service-lifecycle design
//! WP6): gateway-only stop, busy refusals under a held transition,
//! lifecycle-aware gateway status and activation completion.

use super::*;
use crate::lifecycle::{Intent, LifecycleState, Origin, TransitionKind, GATEWAY, POSTGRES};

/// A state over a scratch data dir whose gateway port nothing listens
/// on (the probes and the port-owner lookups stay hermetic).
fn gw_state(name: &str) -> (Arc<AppState>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-gw-ops-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = AppConfig::default_config();
    cfg.data_dir = dir.clone();
    cfg.gateway.port = 40097;
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    (state, dir)
}

fn sink() -> Arc<dyn EventSink> {
    Arc::new(NoopSink)
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Journal one gateway-scope change (no gateway has ever started in
/// the scratch dir, so it stays unresolved).
async fn journal_gateway_change(state: &AppState) {
    let cfg = state.config.read().await.clone();
    activation::record_change(
        &state.data_dir,
        &cfg,
        &cfg,
        "gateway_config",
        "test gateway change".to_string(),
        "gateway",
        Vec::new(),
        &HashMap::new(),
        &std::collections::HashSet::new(),
        &std::collections::HashSet::new(),
    )
    .unwrap();
}

#[tokio::test]
async fn gateway_stop_leaves_postgres_alone_by_default() {
    let (state, dir) = gw_state("stop");
    let sink = sink();
    let v = op_gateway_stop(&state, &sink, None, None).await.unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["stopped"], false);
    assert_eq!(v["postgres_stopped"], false);
    // Intent recorded, transition released.
    assert_eq!(state.lifecycle.intent(GATEWAY), Some(Intent::Stopped));
    assert!(state.lifecycle.transition(GATEWAY).is_none());
    // The gateway's own log tells the story ...
    let gw = read(&lifecycle::log_path(&state.data_dir, GATEWAY));
    assert!(gw.contains("gateway: stop requested"), "{gw}");
    assert!(gw.contains("was not running"), "{gw}");
    // ... and PostgreSQL was never touched.
    let pg = read(&lifecycle::log_path(&state.data_dir, POSTGRES));
    assert!(!pg.contains("stopped"), "{pg}");
    assert!(state.lifecycle.transition(POSTGRES).is_none());
    assert_eq!(state.lifecycle.intent(POSTGRES), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gateway_ops_are_busy_while_a_gateway_transition_is_held() {
    let (state, dir) = gw_state("busy");
    let sink = sink();
    let held = state
        .lifecycle
        .try_begin(GATEWAY, TransitionKind::Restarting, Origin::Activation)
        .unwrap();
    let err = op_gateway_restart(&state, &sink, Some("ui".to_string()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Busy(_)), "{err}");
    let err = op_gateway_stop(&state, &sink, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Busy(_)), "{err}");
    // The wizard step reports the busy payload instead of running.
    let v = op_run_wizard_step(&state, &sink, "start-gateway".to_string(), None)
        .await
        .unwrap();
    assert_eq!(v["busy"], true);
    assert_eq!(v["ok"], false);
    assert_eq!(v["running_step"], "gateway restarting (activation)");
    assert!(v["outcome"].is_null());
    // The database cannot be stopped under a gateway operation either.
    let err = op_postgres_stop(&state, &sink, None).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidInput(_)), "{err}");
    // Nothing the refusals did ended the held transition.
    assert_eq!(
        state.lifecycle.transition(GATEWAY).map(|t| t.origin),
        Some(Origin::Activation)
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gateway_status_reads_restarting_and_keeps_the_plan_pending() {
    let (state, dir) = gw_state("status");
    let sink = sink();
    journal_gateway_change(&state).await;
    // Down, no operation: a gateway-scope change is already effective
    // at the next start, so nothing is pending.
    let v = op_gateway_status(&state, &sink).await.unwrap();
    assert_eq!(v["up"], false);
    assert_eq!(v["state"], "stopped");
    assert_eq!(v["activation_pending"], false);
    assert_eq!(v["postgres"]["state"], "not_installed");
    assert!(v["activation_applying"].is_null());
    // Mid-restart: the status says so and the plan does not vanish.
    let held = state
        .lifecycle
        .try_begin(GATEWAY, TransitionKind::Restarting, Origin::Ui)
        .unwrap();
    let v = op_gateway_status(&state, &sink).await.unwrap();
    assert_eq!(v["up"], false);
    assert_eq!(v["state"], "restarting");
    assert_eq!(v["transition"]["kind"], "restarting");
    assert_eq!(v["transition"]["origin"], "ui");
    assert_eq!(v["activation_pending"], true);
    assert_eq!(v["activation_changes"], 1);
    let plan = op_gateway_activation_plan(&state, &sink).await.unwrap();
    assert_eq!(plan["pending"], true);
    assert_eq!(plan["gateway_up"], false);
    assert_eq!(plan["gateway_state"], "restarting");
    assert_eq!(plan["changes"][0]["activation"], "gateway_restart");
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn activation_complete_does_not_clear_while_the_gateway_is_down() {
    let (state, dir) = gw_state("complete");
    let sink = sink();
    journal_gateway_change(&state).await;
    let v = op_gateway_activation_complete(&state, &sink).await.unwrap();
    assert_eq!(v["cleared"], false);
    assert_eq!(v["reason"], "gateway is not running");
    assert_eq!(activation::load_journal(&state.data_dir).entries.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn install_llamacpp_is_refused_while_a_backend_is_busy() {
    let (state, dir) = gw_state("llamacpp");
    let sink = sink();
    ensure_catalog_models(&state).await;
    let Some(id) = state
        .config
        .read()
        .await
        .backends
        .first()
        .map(|b| b.id.clone())
    else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let held = state
        .lifecycle
        .try_begin(
            &lifecycle::backend_key(&id),
            TransitionKind::Starting,
            Origin::Ui,
        )
        .unwrap();
    let err = op_run_wizard_step(&state, &sink, "install-llamacpp".to_string(), None)
        .await
        .unwrap_err();
    match err {
        ApiError::InvalidInput(msg) => {
            assert!(
                msg.contains(&id) && msg.contains("before updating llama.cpp"),
                "{msg}"
            )
        }
        other => panic!("expected InvalidInput, got {other}"),
    }
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wizard_run_payload_carries_ok() {
    let mut run = WizardRun {
        task_id: "t1".to_string(),
        busy: None,
        outcome: Some(StepOutcome::done("ready", Vec::new())),
        pending_before: true,
    };
    assert!(run.ok());
    let v = run.payload();
    assert_eq!(v["ok"], true);
    assert_eq!(v["task_id"], "t1");
    assert_eq!(v["outcome"]["message"], "ready");
    run.outcome = Some(StepOutcome::failed("boom", Vec::new()));
    assert!(!run.ok());
    assert_eq!(run.payload()["ok"], false);
    let r = gateway_restart_payload(&run);
    assert_eq!(r["ok"], false);
    assert_eq!(r["message"], "boom");
    assert_eq!(r["activation_pending_before"], true);
    // The shared outcome rule reads both as failures.
    assert_eq!(lifecycle::payload_failure(&r).as_deref(), Some("boom"));
    let busy = WizardRun {
        task_id: "t2".to_string(),
        busy: Some(wizard_busy_payload(
            "t2",
            Some(("detect-hardware".to_string(), 3)),
        )),
        outcome: None,
        pending_before: false,
    };
    assert!(!busy.ok());
    let r = gateway_restart_payload(&busy);
    assert_eq!(r["busy"], true);
    assert_eq!(r["ok"], false);
    assert_eq!(r["message"], "busy: detect-hardware is running");
}

#[test]
fn gateway_restart_execution_records_the_gateway_entries() {
    let at = chrono::Utc::now();
    let entry = activation::ActivationEntry {
        ts: "2026-11-12T10:00:00+00:00".to_string(),
        kind: "gateway_config".to_string(),
        summary: "routing rule added".to_string(),
        config_after: Value::Null,
        scope: "gateway".to_string(),
        targets: Vec::new(),
        pre_change_yaml_hash: None,
    };
    let before = GatewayPlanBefore {
        pending: true,
        drift: false,
        gateway_entries: vec![entry],
        started_at: Some(at - chrono::Duration::seconds(30)),
    };
    let ex = gateway_restart_execution(Origin::Wizard, &before, at);
    assert_eq!(ex.source, "wizard");
    assert_eq!(ex.status, "completed");
    assert_eq!(ex.items.len(), 1);
    assert_eq!(ex.items[0].outcome, "applied");
    assert_eq!(ex.items[0].activation, "gateway_restart");
    assert_eq!(ex.outcome_counts(), (1, 0, 0));
    // Drift alone still leaves a record with one item.
    let drift_only = GatewayPlanBefore {
        pending: true,
        drift: true,
        gateway_entries: Vec::new(),
        started_at: None,
    };
    let ex = gateway_restart_execution(Origin::Dashboard, &drift_only, at);
    assert_eq!(ex.items.len(), 1);
    assert_eq!(ex.items[0].kind, "gateway_drift");
    assert!(ex.drift);
}

#[tokio::test]
async fn postgres_stop_runs_under_its_own_transition_when_the_gateway_is_down() {
    let (state, dir) = gw_state("pgstop");
    let sink = sink();
    let v = op_postgres_stop(&state, &sink, Some("cli".to_string()))
        .await
        .unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["stopped"], false, "not installed: nothing to stop");
    assert_eq!(state.lifecycle.intent(POSTGRES), Some(Intent::Stopped));
    let pg = read(&lifecycle::log_path(&state.data_dir, POSTGRES));
    assert!(pg.contains("postgres: stop requested"), "{pg}");
    assert!(pg.contains("by cli"), "{pg}");
    assert_eq!(
        lifecycle::gateway_view(&state).await.state,
        LifecycleState::Stopped
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gateway_start_rearms_postgres_after_a_database_stop() {
    let (state, dir) = gw_state("pgrearm");
    let sink = sink();
    op_postgres_stop(&state, &sink, None).await.unwrap();
    assert_eq!(state.lifecycle.intent(POSTGRES), Some(Intent::Stopped));
    // While stopped by the operator a dead postmaster is just stopped.
    assert_eq!(
        lifecycle::derive_postgres_state(true, None, false, false, false, false),
        LifecycleState::Stopped
    );
    state
        .lifecycle
        .set_failure(POSTGRES, lifecycle::Failure::now("old".to_string(), None));
    // A start-gateway that finished Done re-arms it ...
    rearm_postgres_after_gateway_start(&state);
    assert_eq!(state.lifecycle.intent(POSTGRES), Some(Intent::Running));
    assert!(state.lifecycle.record(POSTGRES).failure.is_none());
    // ... so a dead postmaster now reads failed (the monitor reports it).
    let expected = state.lifecycle.intent(POSTGRES) == Some(Intent::Running);
    assert_eq!(
        lifecycle::derive_postgres_state(true, None, false, false, expected, false),
        LifecycleState::Failed
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn begin_keeps_the_previous_failure_until_the_operation_succeeds() {
    let (state, dir) = gw_state("beginfail");
    let sink = sink();
    state.lifecycle.set_intent(GATEWAY, Intent::Running);
    state
        .lifecycle
        .set_failure(GATEWAY, lifecycle::Failure::now("boom".to_string(), None));
    // A start that is then refused / abandoned (guard dropped without
    // finish) leaves the failed gateway reading failed.
    let g = lifecycle::begin(
        &state,
        &sink,
        GATEWAY,
        TransitionKind::Starting,
        Origin::Ui,
        None,
    )
    .await
    .unwrap();
    drop(g);
    assert!(state.lifecycle.record(GATEWAY).failure.is_some());
    assert_eq!(
        lifecycle::gateway_view(&state).await.state,
        LifecycleState::Failed
    );
    // A successful finish clears it.
    let g = lifecycle::begin(
        &state,
        &sink,
        GATEWAY,
        TransitionKind::Starting,
        Origin::Ui,
        None,
    )
    .await
    .unwrap();
    lifecycle::finish(&state, sink.as_ref(), g, Ok(String::new()));
    assert!(state.lifecycle.record(GATEWAY).failure.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
