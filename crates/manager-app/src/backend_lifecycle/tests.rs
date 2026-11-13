use super::*;
use crate::lifecycle::{BackendLive, ServiceRecord, Transition};
use crate::NoopSink;
use chrono::Utc;
use manager_services::ServiceState;
use manager_supervisor::{ProcessSpec, ProcessState, ProcessStatus};
use std::collections::HashMap;
use std::path::PathBuf;

fn test_state(edit: impl FnOnce(&mut manager_config::AppConfig)) -> (Arc<AppState>, PathBuf) {
    // A counter beside the timestamp: parallel tests can read the
    // same coarse Windows clock tick and would share (and delete)
    // one data dir.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-backend-lifecycle-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    edit(&mut cfg);
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    (state, dir)
}

fn sink() -> Arc<dyn EventSink> {
    Arc::new(NoopSink)
}

fn live_row(pid: Option<u32>, transition: Option<TransitionKind>) -> BackendLive {
    BackendLive {
        id: "a".to_string(),
        state: LifecycleState::Starting,
        pid,
        child_pid: None,
        started_at: None,
        uptime_secs: None,
        service_managed: false,
        adopted: false,
        restarts: 0,
        failure_reason: None,
        transition: transition.map(|kind| Transition {
            kind,
            origin: Origin::Ui,
            since: Utc::now(),
            seq: 1,
        }),
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn slots_busy_reads_is_processing() {
    assert!(!slots_busy(&json!([])));
    assert!(!slots_busy(&json!([{ "id": 0, "is_processing": false }])));
    assert!(slots_busy(&json!([
        { "id": 0, "is_processing": false },
        { "id": 1, "is_processing": true }
    ])));
    // Not a slots array (disabled endpoint, error body): not busy.
    assert!(!slots_busy(&json!({ "error": "slots disabled" })));
}

#[test]
fn readiness_gone_rules() {
    assert!(readiness_gone(None, None).is_err());
    assert_eq!(readiness_gone(Some(&live_row(Some(7), None)), None), Ok(7));
    assert_eq!(readiness_gone(Some(&live_row(Some(7), None)), Some(7)), Ok(7));
    // The process waited on was replaced.
    let replaced = readiness_gone(Some(&live_row(Some(8), None)), Some(7)).unwrap_err();
    assert!(replaced.contains("replaced"), "{replaced}");
    // Exited: the recorded reason is carried.
    let mut gone = live_row(None, None);
    gone.failure_reason = Some("spawn failed: port 8081 in use".to_string());
    let reason = readiness_gone(Some(&gone), Some(7)).unwrap_err();
    assert!(reason.contains("port 8081 in use"), "{reason}");
    // An operator stop ends the wait; a restart in flight does not.
    assert!(
        readiness_gone(Some(&live_row(Some(7), Some(TransitionKind::Stopping))), None).is_err()
    );
    assert_eq!(
        readiness_gone(Some(&live_row(Some(7), Some(TransitionKind::Restarting))), None),
        Ok(7)
    );
}

/// A removed-but-registered backend detached for its stop stays a row
/// ("stopping") instead of vanishing mid-operation.
#[test]
fn merge_keeps_an_orphan_with_an_operation_in_flight() {
    let sup = vec![ProcessStatus {
        id: "gone".to_string(),
        state: ProcessState::Stopped,
        pid: None,
        adopted: false,
        restarts: 0,
        uptime: None,
        failure_reason: None,
    }];
    let mut records: HashMap<String, ServiceRecord> = HashMap::new();
    let no_start = |_: u32| None;
    let without = lifecycle::merge_backend_liveness(
        &sup,
        &HashMap::new(),
        &records,
        &[],
        false,
        Utc::now(),
        &no_start,
    );
    assert!(without.is_empty());
    records.insert(
        backend_key("gone"),
        ServiceRecord {
            transition: Some(Transition {
                kind: TransitionKind::Stopping,
                origin: Origin::Ui,
                since: Utc::now(),
                seq: 1,
            }),
            ..Default::default()
        },
    );
    let with = lifecycle::merge_backend_liveness(
        &sup,
        &HashMap::new(),
        &records,
        &[],
        false,
        Utc::now(),
        &no_start,
    );
    assert_eq!(with["gone"].state, LifecycleState::Stopping);
}

/// ACT-4 / SCM-4: a running service-managed backend (no supervisor
/// process at all) counts as running for the activation plan, with a
/// start instant from its process creation time.
#[test]
fn running_ids_include_a_service_managed_backend() {
    let name = manager_services::backend_service_name("svc");
    let scm: HashMap<String, ServiceStatus> = HashMap::from([(
        name.clone(),
        ServiceStatus {
            name,
            state: ServiceState::Running,
            pid: Some(100),
            child_pid: Some(101),
            win32_exit_code: 0,
            service_specific_exit_code: 0,
        },
    )]);
    let started = Utc::now() - chrono::Duration::seconds(90);
    let start_of = move |pid: u32| (pid == 101).then_some(started);
    let configured = vec![("svc".to_string(), 8081u16), ("off".to_string(), 8082u16)];
    let mut records: HashMap<String, ServiceRecord> = HashMap::new();
    // Model still loading: live (starting), already counted.
    let live = lifecycle::merge_backend_liveness(
        &[],
        &scm,
        &records,
        &configured,
        true,
        Utc::now(),
        &start_of,
    );
    assert_eq!(live["svc"].state, LifecycleState::Starting);
    let ids = crate::running_ids_of(&live);
    assert!(ids.contains("svc") && !ids.contains("off"), "{ids:?}");
    // Ready: running, start instant from the child's creation time.
    records.insert(
        backend_key("svc"),
        ServiceRecord {
            ready_pid: Some(101),
            ..Default::default()
        },
    );
    let live = lifecycle::merge_backend_liveness(
        &[],
        &scm,
        &records,
        &configured,
        true,
        Utc::now(),
        &start_of,
    );
    assert_eq!(live["svc"].state, LifecycleState::Running);
    assert!(live["svc"].service_managed);
    assert_eq!(live["svc"].started_at, Some(started));
    assert!(crate::running_ids_of(&live).contains("svc"));
}

/// BE-1 / BE-2 / ACT-7: while a restart is in flight every surface reads
/// `restarting` (registry, liveness, list rows, running ids), a second
/// restart is refused as busy, and the failed spawn settles the row as
/// failed with its reason and markers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_reads_restarting_in_flight_and_a_second_restart_is_busy() {
    // Hold the backend's port so the restart's spawn waits for it (the
    // port wait runs with no supervisor lock held).
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let (state, dir) = test_state(|cfg| {
        cfg.backends.truncate(1);
        cfg.backends[0].port = port;
    });
    let id = state.config.read().await.backends[0].id.clone();
    let key = backend_key(&id);
    let sink = sink();
    let opts = BackendOpOpts {
        preflight: false,
        graceful: true,
        ..BackendOpOpts::new(Origin::Activation)
    };
    let task = {
        let (state, sink, id) = (Arc::clone(&state), Arc::clone(&sink), id.clone());
        tokio::spawn(async move { restart_backend(&state, &sink, &id, opts).await })
    };
    // The transition is visible a moment before the op records its
    // intent (intent follows a begin that was not refused).
    wait_until("the restart transition", || {
        state.lifecycle.transition(&key).is_some()
            && state.lifecycle.intent(&key) == Some(Intent::Running)
    })
    .await;
    let t = state.lifecycle.transition(&key).unwrap();
    assert_eq!(t.kind, TransitionKind::Restarting);
    assert_eq!(t.origin, Origin::Activation);
    assert_eq!(state.lifecycle.intent(&key), Some(Intent::Running));

    let live = lifecycle::backend_liveness(&state).await;
    assert_eq!(live[&id].state, LifecycleState::Restarting);
    let cfg = state.config.read().await.clone();
    assert!(crate::backend_running_ids(&state, &cfg).await.contains(&id));

    let busy = restart_backend(&state, &sink, &id, opts).await.unwrap_err();
    assert!(matches!(busy, ApiError::Busy(_)), "{busy:?}");
    assert!(
        busy.to_string().contains("restart already in progress"),
        "{busy}"
    );

    let rows = crate::op_list_backends(&state, &sink).await.unwrap();
    let row = rows["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap()
        .clone();
    assert_eq!(row["state"], "restarting");
    assert_eq!(row["lifecycle"], "restarting");
    assert_eq!(row["transition"]["kind"], "restarting");
    assert_eq!(row["transition"]["origin"], "activation");

    // Release the port: the spawn proceeds and fails (there is no
    // llama-server in the scratch data dir).
    drop(holder);
    let result = task.await.unwrap();
    assert!(result.is_err(), "{result:?}");
    assert!(state.lifecycle.transition(&key).is_none());
    let live = lifecycle::backend_liveness(&state).await;
    assert_eq!(live[&id].state, LifecycleState::Failed);
    assert!(live[&id].failure_reason.is_some());
    let log = std::fs::read_to_string(lifecycle::log_path(&state.data_dir, &key)).unwrap();
    assert!(log.contains("restart requested"), "{log}");
    assert!(log.contains("by activation"), "{log}");
    assert!(log.contains("restart failed"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// BE-1 / BE-5: the supervisor mutex is not held while a stop drains, so
/// list_backends (and every other reader) completes during the drain;
/// the stop records intent Stopped first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_drain_does_not_hold_the_supervisor_lock() {
    let (state, dir) = test_state(|_| {});
    let id = "sleeper";
    let (program, args) = if cfg!(windows) {
        // ping ignores the graceful Ctrl-Break, so the drain runs its
        // whole window before the kill.
        (
            "ping",
            vec!["-n".to_string(), "30".to_string(), "127.0.0.1".to_string()],
        )
    } else {
        ("sleep", vec!["30".to_string()])
    };
    {
        let mut sup = state.supervisor().await.lock().await;
        sup.register_spec(ProcessSpec {
            id: id.to_string(),
            program: program.into(),
            args,
            env: vec![],
            workdir: None,
            port: None,
            low_priority: false,
        });
        sup.spawn_backend(id).await.unwrap();
    }
    state
        .backend_spec_ids
        .lock()
        .unwrap()
        .insert(id.to_string());
    let sink = sink();
    let key = backend_key(id);
    let task = {
        let (state, sink) = (Arc::clone(&state), Arc::clone(&sink));
        let opts = BackendOpOpts {
            graceful: true,
            ..BackendOpOpts::new(Origin::Ui)
        };
        tokio::spawn(async move { stop_backend(&state, &sink, id, opts).await })
    };
    // The transition becomes visible a moment before stop_backend
    // records the intent: wait for both, not just the transition.
    wait_until("the stop transition (with its intent) or its end", || {
        (state.lifecycle.transition(&key).is_some()
            && state.lifecycle.intent(&key) == Some(Intent::Stopped))
            || task.is_finished()
    })
    .await;
    assert_eq!(state.lifecycle.intent(&key), Some(Intent::Stopped));
    // Readers are not blocked by the drain.
    let locked = tokio::time::timeout(Duration::from_secs(1), async {
        drop(state.supervisor().await.lock().await);
    })
    .await;
    assert!(locked.is_ok(), "the supervisor mutex is held during the drain");
    let listed =
        tokio::time::timeout(Duration::from_secs(5), crate::op_list_backends(&state, &sink)).await;
    assert!(
        matches!(listed, Ok(Ok(_))),
        "list_backends blocked during the drain"
    );
    task.await.unwrap().unwrap();
    assert!(state.lifecycle.transition(&key).is_none());
    let log = std::fs::read_to_string(lifecycle::log_path(&state.data_dir, &key)).unwrap();
    assert!(log.contains("stop requested"), "{log}");
    assert!(log.contains("stopped"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}
