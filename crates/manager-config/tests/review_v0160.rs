//! Regression tests for code review v0.1.60 (docs/code-review-v0.1.60.md):
//! F1 (default LiteLLM pin + load-time backfill) and F2 (startup
//! recovery preserves unreadable config files before anything saves).

use manager_config::*;
use std::fs;
use std::path::{Path, PathBuf};

fn tmpdir(name: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!(
        "manager-config-review-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn bak_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

fn litellm_pin(cfg: &AppConfig) -> Option<String> {
    cfg.updates
        .components
        .get(LITELLM_COMPONENT_ID)
        .and_then(|c| c.pinned_version.clone())
}

// ---------------------------------------------------------------- F1

#[test]
fn f1_default_config_pins_litellm() {
    let cfg = AppConfig::default_config();
    assert_eq!(litellm_pin(&cfg).as_deref(), Some(DEFAULT_LITELLM_PIN));
    assert_eq!(DEFAULT_LITELLM_PIN, "1.105.1");
    assert!(validate(&cfg).is_ok());
}

#[test]
fn f1_load_backfills_a_file_without_a_litellm_component() {
    let dir = tmpdir("f1-missing");
    let path = dir.join("config.json");
    let mut v = serde_json::to_value(AppConfig::default_config()).unwrap();
    v["updates"]["components"] = serde_json::json!({});
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let cfg = load(&path).unwrap();
    assert_eq!(litellm_pin(&cfg).as_deref(), Some(DEFAULT_LITELLM_PIN));
}

#[test]
fn f1_load_backfills_an_unpinned_litellm_component_and_keeps_its_other_fields() {
    let dir = tmpdir("f1-unpinned");
    let path = dir.join("config.json");
    let mut v = serde_json::to_value(AppConfig::default_config()).unwrap();
    v["updates"]["components"] = serde_json::json!({
        "litellm": { "pinned_version": null, "auto_update": true, "last_checked": null },
        "llamacpp": { "pinned_version": null, "auto_update": true, "last_checked": null }
    });
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let cfg = load(&path).unwrap();
    let comp = &cfg.updates.components[LITELLM_COMPONENT_ID];
    assert_eq!(comp.pinned_version.as_deref(), Some(DEFAULT_LITELLM_PIN));
    assert!(comp.auto_update, "only the pin is backfilled");
    // Other components are never touched by the LiteLLM backfill.
    assert_eq!(cfg.updates.components["llamacpp"].pinned_version, None);
}

#[test]
fn f1_operator_pin_is_never_overridden() {
    let mut cfg = AppConfig::default_config();
    cfg.updates
        .components
        .get_mut(LITELLM_COMPONENT_ID)
        .unwrap()
        .pinned_version = Some("1.104.0".to_string());
    assert!(!backfill_litellm_pin(&mut cfg));
    assert_eq!(litellm_pin(&cfg).as_deref(), Some("1.104.0"));

    let dir = tmpdir("f1-operator");
    let path = dir.join("config.json");
    save(&cfg, &path).unwrap();
    assert_eq!(litellm_pin(&load(&path).unwrap()).as_deref(), Some("1.104.0"));
}

#[test]
fn f1_blank_pin_counts_as_unpinned() {
    let mut cfg = AppConfig::default_config();
    cfg.updates
        .components
        .get_mut(LITELLM_COMPONENT_ID)
        .unwrap()
        .pinned_version = Some("  ".to_string());
    assert!(backfill_litellm_pin(&mut cfg));
    assert_eq!(litellm_pin(&cfg).as_deref(), Some(DEFAULT_LITELLM_PIN));
}

// ---------------------------------------------------------------- F2

#[test]
fn f2_first_run_is_not_a_recovery_and_writes_nothing() {
    let dir = tmpdir("f2-first-run");
    let path = dir.join("config.json");
    let out = load_for_startup(&path, 1_700_000_000_000);
    assert!(out.recovery.is_none());
    assert_eq!(out.config.version, CURRENT_VERSION);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "first run must not create files");
}

#[test]
fn f2_healthy_primary_is_not_a_recovery() {
    let dir = tmpdir("f2-healthy");
    let path = dir.join("config.json");
    save(&AppConfig::default_config(), &path).unwrap();
    let out = load_for_startup(&path, 1);
    assert!(out.recovery.is_none());
}

#[test]
fn f2_both_files_unreadable_preserves_both_before_defaults() {
    let dir = tmpdir("f2-both-bad");
    let path = dir.join("config.json");
    let bak = bak_of(&path);
    fs::write(&path, b"{ this is not json").unwrap();
    fs::write(&bak, b"also garbage").unwrap();
    let now = 1_762_000_000_123;
    let out = load_for_startup(&path, now);
    let rec = out.recovery.expect("both files unreadable is a recovery");
    assert_eq!(rec.kind, ConfigRecoveryKind::Defaults);
    assert!(!rec.error.is_empty());
    let primary_copy = dir.join(format!("config.json.corrupt-{now}"));
    let bak_copy = dir.join(format!("config.json.bak.corrupt-{now}"));
    assert_eq!(fs::read(&primary_copy).unwrap(), b"{ this is not json");
    assert_eq!(fs::read(&bak_copy).unwrap(), b"also garbage");
    assert_eq!(rec.preserved, vec![primary_copy.clone(), bak_copy.clone()]);
    assert!(rec.preservation_errors.is_empty());

    // The regression itself: the next save (startup catalog seeding
    // does one) rotates the corrupt primary over .bak and writes the
    // defaults — the preserved copies must survive that untouched.
    save(&out.config, &path).unwrap();
    save(&out.config, &path).unwrap();
    assert_eq!(fs::read(&primary_copy).unwrap(), b"{ this is not json");
    assert_eq!(fs::read(&bak_copy).unwrap(), b"also garbage");
}

#[test]
fn f2_backup_fallback_is_reported_and_preserves_the_bad_primary() {
    let dir = tmpdir("f2-bak-used");
    let path = dir.join("config.json");
    let mut good = AppConfig::default_config();
    good.gateway.port = 4111;
    save(&good, &path).unwrap();
    // Second save rotates the good file to .bak; then corrupt the primary.
    save(&good, &path).unwrap();
    fs::write(&path, b"truncated{").unwrap();
    let out = load_for_startup(&path, 42);
    let rec = out.recovery.expect("a .bak fallback is a recovery");
    assert_eq!(rec.kind, ConfigRecoveryKind::UsedBackup);
    assert_eq!(out.config.gateway.port, 4111);
    let copy = dir.join("config.json.corrupt-42");
    assert_eq!(fs::read(&copy).unwrap(), b"truncated{");
    assert_eq!(rec.preserved, vec![copy]);
}

#[test]
fn f2_missing_primary_with_unreadable_bak_preserves_the_bak() {
    let dir = tmpdir("f2-bak-only");
    let path = dir.join("config.json");
    fs::write(bak_of(&path), b"[]").unwrap();
    let out = load_for_startup(&path, 7);
    let rec = out.recovery.expect("an unreadable .bak with no primary is a recovery");
    assert_eq!(rec.kind, ConfigRecoveryKind::Defaults);
    assert_eq!(rec.preserved, vec![dir.join("config.json.bak.corrupt-7")]);
}

#[test]
fn f2_preserve_helper_copies_only_existing_files_and_never_overwrites() {
    let dir = tmpdir("f2-helper");
    let path = dir.join("config.json");
    // Nothing exists: nothing copied, no error.
    let r = preserve_unreadable_config(&path, true, 5);
    assert!(r.preserved.is_empty() && r.errors.is_empty());
    // Primary only.
    fs::write(&path, b"one").unwrap();
    let r = preserve_unreadable_config(&path, true, 5);
    assert_eq!(r.preserved, vec![dir.join("config.json.corrupt-5")]);
    // Same timestamp again: an existing copy is never clobbered; the
    // helper picks a fresh suffix instead.
    fs::write(&path, b"two").unwrap();
    let r = preserve_unreadable_config(&path, false, 5);
    assert_eq!(r.preserved.len(), 1);
    assert_ne!(r.preserved[0], dir.join("config.json.corrupt-5"));
    assert_eq!(fs::read(dir.join("config.json.corrupt-5")).unwrap(), b"one");
    assert_eq!(fs::read(&r.preserved[0]).unwrap(), b"two");
}
