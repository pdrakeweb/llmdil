//! Test helpers shared by the unit tests of several `lib.rs` op modules.

use crate::*;

pub(crate) fn test_cfg() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.data_dir = PathBuf::from("/data");
    cfg
}

/// A fresh scratch directory name: pid + a process-wide counter +
/// the clock. The clock alone collided between tests started in
/// parallel, letting two tests share (and one delete) a data dir.
pub(crate) fn unique_test_dir(prefix: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{n}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// An AppState over a temp dir for the advanced-flags op
/// tests (tracker #60): config file and data dir in scratch
/// space, so saves and the activation journal land there.
pub(crate) fn advanced_test_state(cfg: AppConfig) -> (Arc<AppState>, PathBuf) {
    let dir = unique_test_dir("llm-mgr-advanced-flags");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = cfg;
    cfg.data_dir = dir.clone();
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    // Pin an empty telemetry store. Left unset, the first
    // `state.telemetry()` call starts the real host poller and
    // an nvidia-smi inventory query, and whichever finishes
    // first decides what the op sees: on a GPU host the GPU
    // count/names arrived mid-test only under load, flipping
    // split checks and measurement fingerprints. A test that
    // needs GPUs writes a fixture inventory into this store.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    (state, dir)
}

pub(crate) fn test_sink() -> Arc<dyn EventSink> {
    Arc::new(NoopSink)
}

pub(crate) fn register_test_model(cfg: &mut AppConfig, model_id: &str, file: &str) {
    cfg.models.push(manager_config::ModelConfig {
        id: model_id.to_string(),
        gguf_path: PathBuf::from(file),
        quant: "Q4_K_M".to_string(),
        params_b: 30.0,
        sha256: "a".repeat(64),
        verified_at: None,
        verified_sha256: None,
        source_url: String::new(),
        assigned_backends: Vec::new(),
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    });
}

pub(crate) fn uncreated_backend(
    id: &str,
    file: &str,
    raw_flags: Option<String>,
) -> manager_config::BackendConfig {
    manager_config::BackendConfig {
        id: id.to_string(),
        enabled: true,
        model_file: PathBuf::from(file),
        port: 8090,
        flags: manager_config::ServerFlags {
            n_ctx: 32768,
            n_batch: 512,
            tensor_split: Vec::new(),
            split_mode: manager_config::SplitMode::None,
        },
        raw_flags,
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    }
}

pub(crate) fn catalog_restrictions(model_id: &str) -> Vec<manager_models::OffloadRestriction> {
    manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
        .unwrap()
        .offload_restrictions
}

pub(crate) fn backend(cfg: &AppConfig, id: &str) -> manager_config::BackendConfig {
    cfg.backends.iter().find(|b| b.id == id).unwrap().clone()
}

pub(crate) fn vram_reading(index: u32, total_mib: u64, free_mib: u64) -> VramGateReading {
    VramGateReading {
        index,
        total_mib,
        free_mib,
        down: false,
        driver_model: None,
    }
}

/// Three fake inventory GPUs with stable UUIDs.
pub(crate) fn test_gpus() -> Vec<GpuInfo> {
    (0..3)
        .map(|i| GpuInfo {
            index: i,
            name: format!("GPU {i}"),
            uuid: format!("GPU-uuid-{i}"),
            compute_capability: (8, 6),
            total_vram_mib: 16384,
            pci_bus_id: format!("00000000:0{}:00.0", i + 1),
            driver_version: "581.57".into(),
            driver_model: None,
            source: manager_telemetry::TelemetrySourceKind::NvidiaSmi,
        })
        .collect()
}

/// Register a catalog model in a test config and point a
/// backend at its file, so model resolution finds it.
pub(crate) fn serve_model(
    cfg: &mut AppConfig,
    backend_id: &str,
    model_id: &str,
    file: &str,
    raw_flags: Option<&str>,
) -> manager_config::BackendConfig {
    cfg.models.push(manager_config::ModelConfig {
        id: model_id.to_string(),
        gguf_path: PathBuf::from(file),
        quant: "Q4_K_M".to_string(),
        params_b: 30.0,
        sha256: "a".repeat(64),
        verified_at: None,
        verified_sha256: None,
        source_url: String::new(),
        assigned_backends: vec![backend_id.to_string()],
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    });
    let b = cfg
        .backends
        .iter_mut()
        .find(|b| b.id == backend_id)
        .unwrap();
    b.model_file = PathBuf::from(file);
    b.raw_flags = raw_flags.map(str::to_string);
    b.clone()
}

/// A minimal GGUF header (directory only — the layout parser
/// reads nothing past it) with `moe_blocks` blocks carrying one
/// routed-expert tensor each, plus `dense_blocks` plain blocks.
pub(crate) fn gguf_header_bytes(moe_blocks: u32, dense_blocks: u32) -> Vec<u8> {
    fn w_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn w_kv_str(out: &mut Vec<u8>, key: &str, v: &str) {
        w_str(out, key);
        out.extend_from_slice(&8u32.to_le_bytes());
        w_str(out, v);
    }
    fn w_kv_u32(out: &mut Vec<u8>, key: &str, v: u32) {
        w_str(out, key);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn w_tensor(out: &mut Vec<u8>, name: &str, elems: u64) {
        w_str(out, name);
        out.extend_from_slice(&1u32.to_le_bytes()); // n_dims
        out.extend_from_slice(&elems.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // F32
        out.extend_from_slice(&0u64.to_le_bytes()); // offset
    }
    let total = moe_blocks + dense_blocks;
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&(total as u64).to_le_bytes()); // tensor count
    b.extend_from_slice(&2u64.to_le_bytes()); // kv count
    w_kv_str(&mut b, "general.architecture", "testarch");
    w_kv_u32(&mut b, "testarch.block_count", total);
    for i in 0..moe_blocks {
        w_tensor(&mut b, &format!("blk.{i}.ffn_gate_exps.weight"), 256);
    }
    for i in 0..dense_blocks {
        w_tensor(
            &mut b,
            &format!("blk.{}.ffn_gate.weight", moe_blocks + i),
            256,
        );
    }
    b
}
