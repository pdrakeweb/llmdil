use super::*;
use crate::test_support::*;

/// Prediction-refresh precondition (the backend card's
/// Refresh control mirrors it): the measure op — the
/// Measure profile, whose entries feed the prediction —
/// refuses a backend that is not running.
#[tokio::test]
async fn optimize_measure_backend_refuses_a_stopped_backend() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_optimize_measure_backend(&state, &sink, "planner".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("not running"), "{msg}");
            assert!(msg.contains("measurements"), "{msg}");
        }
        other => panic!("expected the not-running refusal, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prediction-refresh precondition: the shared GPU-job
/// slot admits one job at a time, so a measure requested
/// while another measurement or experiment job runs is
/// refused — the refusal the card surfaces instead of
/// failing silently.
#[tokio::test]
async fn optimize_measure_backend_refuses_while_a_gpu_job_runs() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    *state.optimize.job_abort.lock().await = Some(tx);
    let r = op_optimize_measure_backend(&state, &sink, "planner".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("already running"), "{msg}")
        }
        other => panic!("expected the job-slot refusal, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The backend card's Predicted throughput field reads the
/// list_backends row: once a benchmark entry exists under a
/// backend's current fingerprint — what a completed Measure
/// (the card's Refresh) appends to the store — the row
/// resolves it as `predicted`, and a backend with no entry
/// carries no `predicted` key (the card renders its empty
/// state for the absent field; the payload never sends a
/// null-as-zero).
#[tokio::test]
async fn list_backends_resolves_predicted_from_a_stored_measurement() {
    let (state, dir) = advanced_test_state(test_cfg());
    // Hermetic telemetry: the fingerprint below is computed with
    // no GPUs, so list_backends must not see live host GPUs.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    let sink = test_sink();
    let cfg = state.config.read().await.clone();
    let planner = cfg
        .backends
        .iter()
        .find(|b| b.id == "planner")
        .expect("default config seeds planner");
    let fp = manager_optimize::measurement_fingerprint(&optimize::fingerprint_inputs(
        &cfg,
        planner,
        &[],
    ));
    let bench = |gen: f64| manager_optimize::BenchResult {
        prompt_per_second: Some(900.0),
        predicted_per_second: Some(gen),
        ttft_ms: Some(200.0),
        runs: Vec::new(),
    };
    let mut store = manager_optimize::MeasurementStore::default();
    store.upsert(manager_optimize::MeasurementEntry {
        fingerprint: fp,
        backend_id: "planner".to_string(),
        model_id: "model".to_string(),
        measured_at: "2026-10-20T00:00:00Z".to_string(),
        app_version: "test".to_string(),
        short: bench(22.5),
        long: bench(21.0),
        source: Some(manager_optimize::MeasurementSource::Benchmark),
        origin_backend_id: None,
        model_file: None,
        model_sha256: None,
        model_size_bytes: None,
        inputs: None,
        driver_version: Vec::new(),
        llama_build: None,
        residency: None,
        peak_temp_c: None,
    });
    store.save(&optimize::measurements_path(&state)).unwrap();
    let r = op_list_backends(&state, &sink).await.unwrap();
    let rows = r["backends"].as_array().unwrap();
    let prow = rows
        .iter()
        .find(|row| row["id"] == json!("planner"))
        .expect("planner row present");
    assert_eq!(prow["predicted"]["predicted_per_second"], json!(22.5));
    assert_eq!(prow["predicted"]["source"], json!("benchmark"));
    let other = cfg
        .backends
        .iter()
        .map(|b| b.id.clone())
        .find(|id| id != "planner")
        .expect("a second configured backend exists");
    let orow = rows
        .iter()
        .find(|row| row["id"] == json!(other))
        .expect("second backend row present");
    assert!(
        orow.get("predicted").is_none(),
        "row for {other} must omit predicted: {orow}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn process_state_str_covers_all_variants() {
    for (v, want) in [
        (ProcessState::Stopped, "stopped"),
        (ProcessState::Starting, "starting"),
        (ProcessState::Running, "running"),
        (ProcessState::Stopping, "stopping"),
        (ProcessState::Failed, "failed"),
    ] {
        assert_eq!(process_state_str(v), want);
    }
}
