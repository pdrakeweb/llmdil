//! Integration tests: save→load round-trip, .bak rotation and fallback,
//! and v0 migration through real files.

use manager_config::*;
use std::fs;
use std::path::PathBuf;

fn tmpdir(name: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!(
        "manager-config-it-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn rich_config() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.view_modes
        .insert("dashboard".to_string(), ViewMode::Topology);
    cfg.models.push(ModelConfig {
        id: "qwen3-8b".to_string(),
        gguf_path: PathBuf::from("models/qwen3-8b.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 8.0,
        sha256: "cd".repeat(32),
        verified_at: None,
        source_url: "https://example.com/qwen3-8b.gguf".to_string(),
        assigned_backends: vec!["tool-runner".to_string()],
        enabled: true,
        display_name: None,
    });
    let backend = cfg
        .backends
        .iter_mut()
        .find(|b| b.id == "tool-runner")
        .unwrap();
    backend.enabled = true;
    backend.model_file = PathBuf::from("models/qwen3-8b.gguf");
    backend.flags.tensor_split = vec![0.6, 0.4];
    backend.flags.split_mode = SplitMode::Layer;
    backend.raw_flags = Some("--mlock".to_string());
    cfg.mxc.mode = MxcMode::Enforced;
    cfg.winml.enabled = true;
    cfg.winml.model_id = Some("qwen3-8b".to_string());
    cfg.updates.components.insert(
        "llamacpp".to_string(),
        ComponentUpdate {
            pinned_version: Some("b6143".to_string()),
            auto_update: false,
            last_checked: None,
        },
    );
    cfg
}

fn config_value(cfg: &AppConfig) -> serde_json::Value {
    serde_json::to_value(cfg).unwrap()
}

#[test]
fn save_load_round_trip_preserves_config() {
    let dir = tmpdir("roundtrip");
    let path = dir.join("config.json");
    let cfg = rich_config();
    save(&cfg, &path).unwrap();
    let loaded = load(&path).unwrap();
    assert_eq!(config_value(&cfg), config_value(&loaded));
}

#[test]
fn bak_rotation_keeps_last_known_good() {
    let dir = tmpdir("rotation");
    let path = dir.join("config.json");
    let bak = dir.join("config.json.bak");

    let first = rich_config();
    save(&first, &path).unwrap();
    assert!(!bak.exists(), "no .bak after first save");

    let mut second = rich_config();
    second.gateway.request_log_len = 250;
    save(&second, &path).unwrap();
    assert!(bak.exists(), ".bak created on second save");

    // .bak holds the first config.
    let bak_cfg = load(&bak).unwrap();
    assert_eq!(config_value(&first), config_value(&bak_cfg));
    // Primary holds the second.
    let cur = load(&path).unwrap();
    assert_eq!(cur.gateway.request_log_len, 250);
}

#[test]
fn load_falls_back_to_bak_when_primary_corrupt() {
    let dir = tmpdir("fallback");
    let path = dir.join("config.json");

    let first = rich_config();
    save(&first, &path).unwrap();
    let mut second = rich_config();
    second.telemetry.poll_interval_ms = 2000;
    save(&second, &path).unwrap();

    // Corrupt the primary; .bak still holds the first config.
    fs::write(&path, "{ this is not json").unwrap();

    let loaded = load(&path).unwrap();
    assert_eq!(config_value(&first), config_value(&loaded));
}

#[test]
fn load_fails_when_both_primary_and_bak_corrupt() {
    let dir = tmpdir("both-corrupt");
    let path = dir.join("config.json");
    let bak = dir.join("config.json.bak");

    save(&rich_config(), &path).unwrap();
    fs::write(&path, "garbage").unwrap();
    fs::write(&bak, "also garbage").unwrap();

    let err = load(&path).unwrap_err().to_string();
    assert!(
        err.contains("expected value") || err.contains("JSON"),
        "unexpected error: {err}"
    );
}

#[test]
fn load_rejects_invalid_primary_and_uses_valid_bak() {
    let dir = tmpdir("invalid-fallback");
    let path = dir.join("config.json");

    let good = rich_config();
    save(&good, &path).unwrap();
    // Second save rotates the first into .bak so a fallback copy exists.
    let mut good2 = rich_config();
    good2.telemetry.poll_interval_ms = 2000;
    save(&good2, &path).unwrap();

    // Write an invalid (duplicate ports) config directly, bypassing save().
    let mut bad = rich_config();
    bad.backends[0].port = bad.backends[1].port;
    let bad_json = serde_json::to_string_pretty(&bad).unwrap();
    fs::write(&path, bad_json).unwrap();

    // load() must fall back to the .bak (which holds `good`, the config
    // that was primary before the second save rotated it).
    let loaded = load(&path).unwrap();
    assert_eq!(config_value(&good), config_value(&loaded));
}

#[test]
fn v0_file_migrates_on_load() {
    let dir = tmpdir("v0");
    let path = dir.join("config.json");
    // Pre-versioned config: no "version" key, sparse fields. Omitted keys
    // (backends, models, ...) are filled from current defaults.
    fs::write(
        &path,
        r#"{
            "data_dir": "/tmp/llm-v0",
            "gateway": { "port": 4000 }
        }"#,
    )
    .unwrap();

    let cfg = load(&path).unwrap();
    assert_eq!(cfg.version, CURRENT_VERSION);
    assert_eq!(cfg.data_dir, PathBuf::from("/tmp/llm-v0"));
    assert_eq!(cfg.gateway.request_log_len, 100);
    assert_eq!(cfg.gateway.groups.len(), 4);
    assert_eq!(cfg.backends.len(), 4);
}

#[test]
fn load_detailed_reports_backup_fallback() {
    let dir = tmpdir("detailed");
    let path = dir.join("config.json");

    let good = rich_config();
    save(&good, &path).unwrap();
    // Second save rotates the first into .bak.
    let mut good2 = rich_config();
    good2.telemetry.poll_interval_ms = 2000;
    save(&good2, &path).unwrap();

    // Healthy primary: no fallback.
    let report = load_detailed(&path).unwrap();
    assert!(!report.used_backup);
    assert_eq!(report.config.telemetry.poll_interval_ms, 2000);

    // Corrupt the primary: fallback used, banner condition visible.
    fs::write(&path, "{ corrupt").unwrap();
    let report = load_detailed(&path).unwrap();
    assert!(report.used_backup);
    assert_eq!(config_value(&good), config_value(&report.config));

    // Plain load() still returns the config without the flag.
    let cfg = load(&path).unwrap();
    assert_eq!(config_value(&good), config_value(&cfg));
}

#[test]
fn save_rejects_invalid_config_without_touching_disk() {
    let dir = tmpdir("save-invalid");
    let path = dir.join("config.json");
    let mut cfg = rich_config();
    cfg.backends[0].port = cfg.backends[1].port;
    let err = save(&cfg, &path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Validation(_)),
        "unexpected: {err}"
    );
    assert!(!path.exists());
}

#[test]
fn save_creates_parent_dirs() {
    let dir = tmpdir("parents");
    let path = dir.join("nested").join("deep").join("config.json");
    save(&rich_config(), &path).unwrap();
    assert!(path.exists());
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.gateway.port, 4000);
}

#[test]
fn unknown_fields_survive_file_round_trip() {
    // A config written by a newer app version (extra fields) must still load;
    // unknown fields are ignored, known fields preserved.
    let dir = tmpdir("unknown-fields");
    let path = dir.join("config.json");
    let mut raw = serde_json::to_value(rich_config()).unwrap();
    raw["future_top_level"] = serde_json::json!({ "x": 1 });
    raw["gateway"]["future_gateway_flag"] = serde_json::json!(true);
    fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

    let cfg = load(&path).unwrap();
    assert_eq!(cfg.gateway.port, 4000);
    assert_eq!(cfg.models.len(), 1);
}
