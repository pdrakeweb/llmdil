//! Golden-payload tests for the read operations the UI, CLI and
//! Android client parse (technical spec §15; admin API plan phases
//! 0/3/4). Each test dispatches one operation by its registry name
//! against a deterministic fixture state and compares the normalised
//! JSON with `tests/golden/<op>.json`. See `tests/support/golden.rs`
//! for the harness and `UPDATE_GOLDEN=1` for regenerating.
//!
//! The fixture: the default configuration with every port moved to
//! an unused high range (so a real backend or gateway on the host is
//! never probed or adopted), the gateway bound to loopback (no LAN
//! address lookup), and a pinned telemetry store holding a fixed
//! two-GPU inventory — the live poller never starts.

use super::*;
use chrono::TimeZone;

#[path = "../tests/support/golden.rs"]
mod golden;

use golden::{assert_golden, Normalizer};

/// Ports no developer service is expected to hold: the gateway at
/// 59_100 and backends from 59_101.
const GATEWAY_PORT: u16 = 59_100;
const FIRST_BACKEND_PORT: u16 = 59_101;

fn fixture_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-golden-{}-{n}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture_config(dir: &std::path::Path) -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.data_dir = dir.to_path_buf();
    cfg.gateway.port = GATEWAY_PORT;
    cfg.gateway.bind_host = "127.0.0.1".to_string();
    for (i, b) in cfg.backends.iter_mut().enumerate() {
        b.port = FIRST_BACKEND_PORT + i as u16;
    }
    cfg
}

fn fixed_ts(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap() + chrono::Duration::seconds(secs)
}

/// A two-GPU store: an RTX 4090 under WDDM and a P100 under TCC, two
/// polls each (so the sparkline history and peak marks are populated)
/// and one process per GPU — one unmeasured, one measured.
fn fixture_telemetry() -> TelemetryStore {
    use manager_telemetry::{
        DriverModel, GpuInfo, GpuSample, PowerAbsence, ProcessSample, TelemetrySourceKind,
    };
    let mut store = TelemetryStore::new();
    store.set_inventory(vec![
        GpuInfo {
            index: 0,
            name: "NVIDIA GeForce RTX 4090".into(),
            uuid: "GPU-00000000-0000-0000-0000-000000000000".into(),
            compute_capability: (8, 9),
            total_vram_mib: 24_564,
            pci_bus_id: "00000000:01:00.0".into(),
            driver_version: "581.57".into(),
            driver_model: Some(DriverModel::Wddm),
            source: TelemetrySourceKind::NvidiaSmi,
        },
        GpuInfo {
            index: 1,
            name: "Tesla P100-PCIE-16GB".into(),
            uuid: "GPU-11111111-1111-1111-1111-111111111111".into(),
            compute_capability: (6, 0),
            total_vram_mib: 16_384,
            pci_bus_id: "00000000:02:00.0".into(),
            driver_version: "581.57".into(),
            driver_model: Some(DriverModel::Tcc),
            source: TelemetrySourceKind::NvidiaSmi,
        },
    ]);
    let poll = |t: i64, util0: f32, temp0: f32| {
        let mut g0 = GpuSample::fresh(
            0,
            "NVIDIA GeForce RTX 4090",
            util0,
            8_192,
            24_564,
            temp0,
            180.5,
            TelemetrySourceKind::NvidiaSmi,
        );
        g0.ts = fixed_ts(t);
        g0.mem_util_pct = Some(12.0);
        g0.fan_pct = Some(40.0);
        g0.power_limit_w = Some(450.0);
        g0.driver_model = Some(DriverModel::Wddm);
        let mut g1 = GpuSample::fresh(
            1,
            "Tesla P100-PCIE-16GB",
            0.0,
            512,
            16_384,
            35.0,
            0.0,
            TelemetrySourceKind::NvidiaSmi,
        );
        g1.ts = fixed_ts(t);
        g1.power_w = None;
        g1.power_absence = Some(PowerAbsence::DriverNotAvailable);
        g1.driver_model = Some(DriverModel::Tcc);
        vec![g0, g1]
    };
    store.update(poll(0, 30.0, 55.0));
    store.update(poll(2, 75.0, 61.0));
    store.update_processes(vec![
        ProcessSample {
            pid: 4_242,
            name: "llama-server.exe".into(),
            vram_mib: None,
            gpu_index: 0,
            backend_id: None,
        },
        ProcessSample {
            pid: 4_343,
            name: "python.exe".into(),
            vram_mib: Some(2_048),
            gpu_index: 1,
            backend_id: None,
        },
    ]);
    store
}

struct Fixture {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture() -> Fixture {
    let dir = fixture_dir();
    let state = Arc::new(AppState::new(
        fixture_config(&dir),
        dir.join("config.json"),
        dir.clone(),
    ));
    // Pin the store before any op runs: left unset, the first
    // `state.telemetry()` call starts the real host poller.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(fixture_telemetry())));
    Fixture { state, dir }
}

async fn run(fx: &Fixture, name: &str) -> Value {
    let op = registry::op_by_name(name).unwrap_or_else(|| panic!("{name} is registered"));
    let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
    registry::dispatch(&fx.state, &sink, op, json!({}))
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"))
}

fn check(fx: &Fixture, name: &str, mut payload: Value, volatile_keys: &[&'static str]) {
    Normalizer::keys(volatile_keys)
        .path(&fx.dir, "<DATA_DIR>")
        // Config-relative paths serialize with the platform separator.
        .separators(&["gguf_path", "model_file", "path"])
        .apply(&mut payload);
    assert_golden(env!("CARGO_MANIFEST_DIR"), name, &payload);
}

#[tokio::test]
async fn golden_get_status() {
    let fx = fixture();
    let v = run(&fx, "get_status").await;
    // The build version changes every release; its presence and type
    // are the contract.
    check(&fx, "get_status", v, &["version"]);
}

#[tokio::test]
async fn golden_list_backends() {
    let fx = fixture();
    // Startup seeding first (as the app does): it registers the
    // catalog models and aligns the default backends to them, so the
    // rows carry the catalog-derived memory, fit (against the fixture
    // GPUs), offload, MTP and cache blocks the UI renders.
    ensure_catalog_models(&fx.state).await;
    let v = run(&fx, "list_backends").await;
    check(&fx, "list_backends", v, &[]);
}

#[tokio::test]
async fn golden_get_gpu_telemetry() {
    let fx = fixture();
    let v = run(&fx, "get_gpu_telemetry").await;
    // at_ms is the store's last-successful-poll wall-clock time.
    check(&fx, "get_gpu_telemetry", v, &["at_ms"]);
}

#[tokio::test]
async fn golden_gateway_status() {
    let fx = fixture();
    let v = run(&fx, "gateway_status").await;
    check(&fx, "gateway_status", v, &[]);
}

#[tokio::test]
async fn golden_list_models() {
    let fx = fixture();
    let v = run(&fx, "list_models").await;
    check(&fx, "list_models", v, &[]);
}
