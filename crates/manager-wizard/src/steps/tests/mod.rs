mod clients_and_misc;
mod gateway_hash_lock;
mod gateway_install;
mod gateway_prisma_mcp;
mod hardware_models;
mod lifecycle;
mod postgres;

use self::clients_and_misc::*;
use self::gateway_install::*;
use self::gateway_prisma_mcp::*;
use self::lifecycle::*;
use self::postgres::*;

use super::*;
use crate::fake::FakeSystemOps;
use crate::{CommandOutput, GpuDescriptor, RunMode, StepState, WizardContext};
use manager_config::{AppConfig, ComponentUpdate, ModelConfig};
use sha2::{Digest, Sha256};
use std::sync::Arc;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[test]
fn gateway_launch_spec_maps_mcp_credentials_to_references() {
    // Code-review B-7: an armed gateway MCP server's credential
    // must reach the service definition as a credential
    // reference, never as a plaintext literal; non-secret env
    // and local-exposure servers' keys stay literal.
    let mut config = AppConfig::default_config();
    let brave_target = config
        .mcp
        .servers
        .iter()
        .find(|s| s.id == "brave_search")
        .and_then(|s| s.cred_ref.clone())
        .expect("curated brave server names a credential");
    config.mcp.servers.push(manager_config::McpServerConfig {
        id: "local_only".to_string(),
        env_keys: vec!["LOCAL_ONLY_KEY".to_string()],
        cred_ref: Some("local-llm-service-manager/local-only".to_string()),
        exposure: manager_config::McpExposure::Local,
        ..Default::default()
    });
    let mut cmd = PlannedCommand::new(vec!["pythonw.exe".to_string()]);
    cmd.env.insert(
        "BRAVE_API_KEY".to_string(),
        "sentinel-mcp-value".to_string(),
    );
    cmd.env
        .insert("LOCAL_ONLY_KEY".to_string(), "local-value".to_string());
    cmd.env
        .insert("PLAIN_SETTING".to_string(), "plain".to_string());
    let spec = gateway_launch_spec(&cmd, &config);
    let brave = spec
        .env
        .iter()
        .find(|e| e.name == "BRAVE_API_KEY")
        .expect("brave env entry present");
    assert_eq!(
        brave.value,
        manager_services::EnvValue::CredentialRef {
            target: brave_target
        }
    );
    let local = spec
        .env
        .iter()
        .find(|e| e.name == "LOCAL_ONLY_KEY")
        .expect("local env entry present");
    assert_eq!(
        local.value,
        manager_services::EnvValue::Literal {
            value: "local-value".to_string()
        }
    );
    let serialized = serde_json::to_string(&spec.env).unwrap();
    assert!(
        !serialized.contains("sentinel-mcp-value"),
        "MCP credential value must not appear in the definition env: {serialized}"
    );
    assert!(serialized.contains("plain"));
}

fn test_gpus() -> Vec<GpuDescriptor> {
    vec![
        GpuDescriptor {
            index: 0,
            name: "NVIDIA RTX A4000".to_string(),
            uuid: "GPU-aaaa".to_string(),
            total_vram_mib: 16384,
            compute_capability: (8, 6),
            driver_version: "581.57".to_string(),
        },
        GpuDescriptor {
            index: 1,
            name: "Tesla P100-PCIE-16GB".to_string(),
            uuid: "GPU-bbbb".to_string(),
            total_vram_mib: 16384,
            compute_capability: (6, 0),
            driver_version: "581.57".to_string(),
        },
        GpuDescriptor {
            index: 2,
            name: "Tesla P100-PCIE-16GB".to_string(),
            uuid: "GPU-cccc".to_string(),
            total_vram_mib: 16384,
            compute_capability: (6, 0),
            driver_version: "581.57".to_string(),
        },
    ]
}

fn test_pin() -> LlamaCppPin {
    LlamaCppPin {
        tag: "b9999".to_string(),
        asset: "llama-b9999-bin-win-cuda-12.4-x64.zip".to_string(),
        sha256: sha256_hex(b"fake-zip-bytes"),
        runtime: LlamaCppAsset {
            asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"fake-cudart-bytes"),
        },
    }
}

/// Script both archive downloads for `pin` under their real URLs.
fn script_pin_downloads(ops: &FakeSystemOps, pin: &LlamaCppPin) {
    ops.with_download(&pin.download_url(), b"fake-zip-bytes".to_vec());
    ops.with_download(&pin.runtime_download_url(), b"fake-cudart-bytes".to_vec());
}

fn test_config_with_models() -> AppConfig {
    let mut c = AppConfig::default_config();
    for b in c.backends.iter_mut() {
        b.enabled = true;
    }
    c.models = vec![
        ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: "models/qwen3-8b.gguf".into(),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: sha256_hex(b"gguf-8b-bytes"),
            verified_at: None,
            verified_sha256: None,
            source_url: "https://example.com/qwen3-8b.gguf".to_string(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        },
        ModelConfig {
            id: "qwen2.5-coder-7b".to_string(),
            gguf_path: "models/qwen2.5-coder-7b.gguf".into(),
            quant: "Q4_K_M".to_string(),
            params_b: 7.0,
            sha256: sha256_hex(b"gguf-7b-bytes"),
            verified_at: None,
            verified_sha256: None,
            source_url: "https://example.com/qwen2.5-coder-7b.gguf".to_string(),
            assigned_backends: vec!["coder-fast".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        },
    ];
    c
}

fn ctx_with(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
    WizardContext::new(test_config_with_models(), PathBuf::from("/data"), mode, ops)
}

/// Serializes every test that swaps the process-global PATH.
/// PATH is shared by all test threads in this process, so two
/// PathGuard tests running in parallel can restore each other's
/// PATH mid-test (the intermittent start-gateway failures: one
/// test resolves uv against another test's PATH). PathGuard
/// holds this lock from construction until drop, so only one
/// swapped PATH is ever live at a time.
static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Guard that swaps PATH for the duration of a test. uv resolution
/// reads the real process PATH, so tests that assert on it must
/// control it. Restores on drop. Holds [`PATH_LOCK`] for its whole
/// lifetime so PATH-swapping tests never overlap.
struct PathGuard {
    old: Option<std::ffi::OsString>,
    // Held purely for its Drop, which releases PATH_LOCK.
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PathGuard {
    fn set_to(dir: &std::path::Path) -> Self {
        // Recover from poisoning: a panicking holder must not
        // wedge every later PATH test in the process.
        let lock = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.as_os_str());
        Self { old, _lock: lock }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.old.take() {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// Temp dir acting as a PATH entry. When `with_uv` is true it contains
/// a fake `uv` binary so uv resolution hits PATH deterministically.
fn path_dir_with_uv(with_uv: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "llm-wiz-uv-{}-{}",
        std::process::id(),
        if with_uv { "hit" } else { "miss" }
    ));
    std::fs::create_dir_all(&dir).unwrap();
    if with_uv {
        let name = if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        };
        std::fs::write(dir.join(name), b"fake-uv").unwrap();
    }
    dir
}

fn smoke_post_response() -> crate::HttpResponse {
    FakeSystemOps::http_ok(r#"{"choices":[{"message":{"content":"ok"}}]}"#)
}

// --- DetectHardware ---
