use super::*;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex as StdMutex;

use manager_telemetry::{GpuSample, TelemetrySourceKind};

use crate::registry::{dispatch, dispatch_from};
use crate::{NoopSink, TaskStatus};

fn test_state(prefix: &str) -> (Arc<AppState>, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-bus-{prefix}-{}-{n}-{}",
        std::process::id(),
        crate::now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    cfg.gateway.groups.push(manager_config::ModelGroup {
        name: "bus-group".into(),
        members: vec!["planner".into()],
        strategy: manager_config::RoutingStrategy::SimpleShuffle,
        fallbacks: vec![],
        purpose: String::new(),
    });
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    (state, dir)
}

/// Records what reached the caller's own sink.
#[derive(Default)]
struct Recording(StdMutex<Vec<(String, Value)>>);

impl EventSink for Recording {
    fn emit(&self, event: &str, payload: Value) {
        self.0.lock().unwrap().push((event.to_string(), payload));
    }
}

fn drain(rx: &mut broadcast::Receiver<BusEvent>) -> Vec<BusEvent> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn named<'a>(events: &'a [BusEvent], name: &str) -> Vec<&'a BusEvent> {
    events.iter().filter(|e| e.event == name).collect()
}

async fn next_named(
    rx: &mut broadcast::Receiver<BusEvent>,
    name: &str,
    within: Duration,
) -> Option<BusEvent> {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Ok(e) if e.event == name => return Some(e),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn desktop_and_api_dispatch_emit_the_same_config_changed() {
    let (state, dir) = test_state("parity");
    let mut rx = state.event_bus.tx.subscribe();
    let desktop_sink = Arc::new(Recording::default());
    let sink: Arc<dyn EventSink> = desktop_sink.clone();

    // Desktop (IPC) path.
    dispatch(
        &state,
        &sink,
        Op::TelemetrySetInterval,
        json!({ "ms": 2000 }),
    )
    .await
    .unwrap();
    // API path.
    let noop: Arc<dyn EventSink> = Arc::new(NoopSink);
    dispatch_from(
        &state,
        &noop,
        Op::TelemetrySetInterval,
        json!({ "ms": 5000 }),
        EventOrigin::Api,
    )
    .await
    .unwrap();

    let events = drain(&mut rx);
    let changed = named(&events, events::CONFIG_CHANGED);
    assert_eq!(changed.len(), 2, "{events:?}");
    assert_eq!(changed[0].origin, EventOrigin::Desktop);
    assert_eq!(changed[1].origin, EventOrigin::Api);
    for e in &changed {
        assert_eq!(e.payload["op"], "telemetry_set_interval");
        assert_eq!(e.payload["sections"], json!(["telemetry"]));
    }
    // The caller's own sink saw it too (the desktop UI contract).
    let saw_locally = desktop_sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|(e, _)| e == events::CONFIG_CHANGED);
    assert!(saw_locally);

    // Re-saving the same value changes nothing: no event.
    dispatch(
        &state,
        &sink,
        Op::TelemetrySetInterval,
        json!({ "ms": 5000 }),
    )
    .await
    .unwrap();
    assert!(named(&drain(&mut rx), events::CONFIG_CHANGED).is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn failed_operations_emit_nothing() {
    let (state, dir) = test_state("fail");
    let mut rx = state.event_bus.tx.subscribe();
    let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
    // Argument validation failure (before the op runs).
    assert!(dispatch(&state, &sink, Op::TelemetrySetInterval, json!({}))
        .await
        .is_err());
    // Failure inside the op.
    assert!(dispatch(
        &state,
        &sink,
        Op::GatewayTestRouting,
        json!({ "request": { "group": "no-such-group" } }),
    )
    .await
    .is_err());
    assert!(drain(&mut rx).is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn gateway_test_routing_emits_the_recorded_decision() {
    let (state, dir) = test_state("routing");
    let mut rx = state.event_bus.tx.subscribe();
    let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
    let decision = dispatch(
        &state,
        &sink,
        Op::GatewayTestRouting,
        json!({ "request": { "group": "bus-group" } }),
    )
    .await
    .unwrap();
    let events = drain(&mut rx);
    let req = named(&events, events::GATEWAY_REQUEST);
    assert_eq!(req.len(), 1, "{events:?}");
    assert_eq!(req[0].payload, decision);
    assert_eq!(req[0].origin, EventOrigin::Desktop);
    // A read-class op: no config snapshot, no config-changed.
    assert!(named(&events, events::CONFIG_CHANGED).is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn tasks_publish_creation_and_settlement() {
    let (state, dir) = test_state("tasks");
    let mut rx = state.event_bus.tx.subscribe();
    let id = with_origin(EventOrigin::Api, state.record_task("bus-test", json!({}))).await;
    state
        .finish_task(&id, TaskStatus::Done, json!({ "ok": true }))
        .await;
    let events = drain(&mut rx);
    let tasks = named(&events, events::TASK_PROGRESS);
    assert_eq!(tasks.len(), 2, "{events:?}");
    assert_eq!(tasks[0].payload["id"], id);
    assert_eq!(tasks[0].payload["status"], "running");
    assert_eq!(tasks[0].origin, EventOrigin::Api);
    assert_eq!(tasks[1].payload["status"], "done");
    assert_eq!(tasks[1].payload["kind"], "bus-test");
    // Outside any dispatch the origin is the app itself.
    assert_eq!(tasks[1].origin, EventOrigin::App);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn failures_and_activation_raise_alerts_completions_do_not() {
    let (state, dir) = test_state("alerts");
    let mut rx = state.event_bus.tx.subscribe();
    use crate::notify::{notify, Category};
    notify(
        &NoopSink,
        &state,
        Category::Failures,
        "Backend stopped",
        "planner exited",
    )
    .await;
    notify(
        &NoopSink,
        &state,
        Category::Activation,
        "Plan applied",
        "ok",
    )
    .await;
    notify(
        &NoopSink,
        &state,
        Category::Operations,
        "Download done",
        "x",
    )
    .await;
    let events = drain(&mut rx);
    let alerts = named(&events, events::ALERT_RAISED);
    assert_eq!(alerts.len(), 2, "{events:?}");
    assert_eq!(alerts[0].payload["category"], "failures");
    assert_eq!(alerts[0].payload["title"], "Backend stopped");
    assert_eq!(alerts[1].payload["category"], "activation");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn nothing_is_built_without_subscribers() {
    let (state, dir) = test_state("idle");
    assert!(!state.event_bus.has_subscribers());
    // Dispatch skips the config snapshot entirely without listeners.
    let watch = before_dispatch(&state, Op::TelemetrySetInterval).await;
    assert!(watch.config_before.is_none());
    // Subscribing elsewhere turns it on.
    let _rx = state.event_bus.tx.subscribe();
    let watch = before_dispatch(&state, Op::TelemetrySetInterval).await;
    assert!(watch.config_before.is_some());
    let watch = before_dispatch(&state, Op::GetStatus).await;
    assert!(watch.config_before.is_none(), "reads never snapshot");
    let _ = std::fs::remove_dir_all(dir);
}

fn gpu(index: u32, used: u64) -> GpuSample {
    GpuSample::fresh(
        index,
        &format!("GPU-{index}"),
        10.0,
        used,
        16384,
        40.0,
        100.0,
        TelemetrySourceKind::Nvml,
    )
}

#[tokio::test]
async fn telemetry_poll_drives_ticks_and_vram_changes() {
    let (state, dir) = test_state("telemetry");
    let store = Arc::new(StdMutex::new(
        manager_telemetry::store::TelemetryStore::new(),
    ));
    assert!(state.telemetry.set(store.clone()).is_ok());
    let mut rx = subscribe(&state);
    assert!(state.event_bus.producers.load(Ordering::SeqCst));

    crate::lock_telemetry(&store).update(vec![gpu(0, 1000)]);
    let tick = next_named(&mut rx, events::TELEMETRY_TICK, Duration::from_secs(5))
        .await
        .expect("telemetry-tick after a poll");
    assert_eq!(tick.origin, EventOrigin::App);
    assert_eq!(tick.payload["samples"][0]["vram_used_mib"], 1000);
    assert!(tick.payload["at_ms"].as_i64().unwrap() > 0);

    // A big move after the baseline is a vram-changed event.
    crate::lock_telemetry(&store).update(vec![gpu(0, 9000)]);
    let change = next_named(&mut rx, events::VRAM_CHANGED, Duration::from_secs(5))
        .await
        .expect("vram-changed");
    assert_eq!(change.payload["index"], 0);
    assert_eq!(change.payload["previous_mib"], 1000);
    assert_eq!(change.payload["delta_mib"], 8000);

    // The producers stop once the last subscriber leaves.
    drop(rx);
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.event_bus.producers.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("producer loop exits without subscribers");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn vram_changes_need_a_baseline_and_a_threshold() {
    let mut base = HashMap::new();
    assert!(
        vram_changes(&mut base, &[gpu(0, 1000)]).is_empty(),
        "baseline"
    );
    // 16384 MiB card: threshold max(256, 2% = 328) = 328.
    assert!(vram_changes(&mut base, &[gpu(0, 1300)]).is_empty());
    let out = vram_changes(&mut base, &[gpu(0, 1400)]);
    assert_eq!(out.len(), 1, "drift accumulates against the last report");
    assert_eq!(out[0]["delta_mib"], 400);
    let out = vram_changes(&mut base, &[gpu(0, 0)]);
    assert_eq!(out[0]["delta_mib"], -1400);
    // Down GPUs are not reported.
    let mut down = gpu(0, 9000);
    down.down = true;
    assert!(vram_changes(&mut base, &[down]).is_empty());
}

#[test]
fn changed_sections_names_differing_top_level_keys() {
    let a = json!({ "telemetry": { "poll_interval_ms": 1000 }, "gateway": { "port": 4000 } });
    let b =
        json!({ "telemetry": { "poll_interval_ms": 5000 }, "gateway": { "port": 4000 }, "new": 1 });
    assert_eq!(changed_sections(&a, &b), vec!["new", "telemetry"]);
    assert!(changed_sections(&a, &a).is_empty());
}

#[test]
fn log_lines_carry_level_stream_and_rate_limit() {
    use manager_supervisor::{LogRecord, LogStream};
    let records = vec![
        LogRecord::new(LogStream::Stderr, "error: failed to load model"),
        LogRecord::new(LogStream::Stdout, "srv  update_slots: all slots are idle"),
    ];
    let out = log_line_payloads("planner", &records);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["scope"], "planner");
    assert_eq!(out[0]["level"], "error");
    assert_eq!(out[0]["stream"], "stderr");
    assert_eq!(out[1]["level"], "info");
    assert_eq!(out[1]["stream"], "stdout");
    assert!(out[1]["ts_ms"].as_u64().unwrap() > 0);

    let flood: Vec<LogRecord> = (0..LOG_LINES_PER_SCOPE_PER_PASS + 7)
        .map(|i| LogRecord::new(LogStream::Stdout, format!("line {i}")))
        .collect();
    let out = log_line_payloads("planner", &flood);
    assert_eq!(out.len(), LOG_LINES_PER_SCOPE_PER_PASS + 1);
    assert_eq!(out[0]["skipped"], 7);
    assert_eq!(out[1]["line"], "line 7", "the newest lines are kept");
}

#[tokio::test]
async fn log_follow_never_initializes_the_supervisor() {
    // With no supervisor initialized the follower observes nothing
    // (and never initializes it on a subscriber's behalf).
    let (state, dir) = test_state("logs");
    let mut follow = LogFollow::default();
    follow.step(&state).await;
    assert!(state.supervisor.get().is_none());
    assert!(!follow.primed);
    let _ = std::fs::remove_dir_all(dir);
}
