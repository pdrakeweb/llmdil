use super::*;
use crate::test_support::*;

/// A config path whose parent is a regular file: every save to it
/// fails (the parent directory cannot be created).
fn unsavable_config_path(dir: &std::path::Path) -> PathBuf {
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    blocker.join("config.json")
}

/// PS-8 (docs/code-review-v0.1.60.md): a failed save leaves the
/// live configuration exactly as it was; a successful one installs
/// the mutation.
#[test]
fn mutate_save_install_installs_only_on_successful_save() {
    let (_state, dir) = advanced_test_state(test_cfg());
    let mut live = test_cfg();
    live.data_dir = dir.clone();
    let before = serde_json::to_value(&live).unwrap();
    let bad = unsavable_config_path(&dir);
    let r = mutate_save_install(&mut live, &bad, |c| c.catalog_revision += 7);
    assert!(r.is_err());
    assert_eq!(serde_json::to_value(&live).unwrap(), before);
    let good = dir.join("config.json");
    let rev = live.catalog_revision;
    mutate_save_install(&mut live, &good, |c| c.catalog_revision += 7).unwrap();
    assert_eq!(live.catalog_revision, rev + 7);
    assert_eq!(
        manager_config::load(&good).unwrap().catalog_revision,
        rev + 7
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-8: catalog seeding whose save fails leaves memory unseeded,
/// matching the disk, instead of running on unsaved seeded rows.
#[tokio::test]
async fn ensure_catalog_models_failed_save_leaves_memory_unchanged() {
    let dir = unique_test_dir("llm-mgr-ps8-seed");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    assert!(cfg.models.is_empty() && !cfg.catalog_seeded);
    let before = serde_json::to_value(&cfg).unwrap();
    let state = Arc::new(AppState::new(cfg, unsavable_config_path(&dir), dir.clone()));
    ensure_catalog_models(&state).await;
    let after = serde_json::to_value(&*state.config.read().await).unwrap();
    assert_eq!(after, before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-3 (docs/code-review-v0.1.60.md): a reload that could only
/// read the `.bak` copy (primary missing mid-rotation, or corrupt)
/// must not replace the live config with the previous one.
#[tokio::test]
async fn reload_from_disk_ignores_backup_fallback() {
    let (state, dir) = advanced_test_state(test_cfg());
    let mut on_disk = state.config.read().await.clone();
    on_disk.optimize_purpose = "from-bak".to_string();
    manager_config::save(&on_disk, &state.config_path).unwrap();
    manager_config::save(&on_disk, &state.config_path).unwrap(); // .bak = from-bak
    std::fs::write(&state.config_path, b"{ torn").unwrap();
    state.config.write().await.optimize_purpose = "live".to_string();
    state.reload_config_from_disk().await;
    assert_eq!(state.config.read().await.optimize_purpose, "live");
    // A clean primary still reloads.
    on_disk.optimize_purpose = "from-primary".to_string();
    manager_config::save(&on_disk, &state.config_path).unwrap();
    state.reload_config_from_disk().await;
    assert_eq!(state.config.read().await.optimize_purpose, "from-primary");
    let _ = std::fs::remove_dir_all(&dir);
}
