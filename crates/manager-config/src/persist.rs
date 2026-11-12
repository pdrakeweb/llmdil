//! Loading and saving: `.bak` fallback, startup load with corrupt-file
//! preservation, and the atomic, serialized save path.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{migrate, validate, AppConfig, ConfigError};

/// Load, migrate, and validate the config at `path`.
///
/// If the primary file is missing, unreadable, unparsable, or fails
/// validation, the `.bak` last-known-good copy is tried. If that also fails,
/// the primary error is returned. A missing file is an I/O error —
/// callers fall back to [`AppConfig::default_config`] on first run.
///
/// Use [`load_detailed`] when the caller needs to know whether the `.bak`
/// fallback was used (spec §10 requires a UI banner in that case).
pub fn load(path: &PathBuf) -> Result<AppConfig, ConfigError> {
    load_detailed(path).map(|r| r.config)
}

/// Result of [`load_detailed`].
#[derive(Debug)]
pub struct LoadReport {
    pub config: AppConfig,
    /// True when the primary file failed and the `.bak` copy was used.
    /// Spec §10: the UI must banner this condition.
    pub used_backup: bool,
}

/// Like [`load`], but also reports whether the `.bak` fallback was used.
pub fn load_detailed(path: &PathBuf) -> Result<LoadReport, ConfigError> {
    match load_one(path) {
        Ok(config) => Ok(LoadReport {
            config,
            used_backup: false,
        }),
        Err(primary_err) => {
            let bak = bak_path(path);
            match load_one(&bak) {
                Ok(config) => Ok(LoadReport {
                    config,
                    used_backup: true,
                }),
                Err(_) => Err(primary_err),
            }
        }
    }
}

/// Where the configuration the app started with came from
/// (see [`load_for_startup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    /// The primary `config.json` loaded cleanly.
    Primary,
    /// The primary failed; the `.bak` last-known-good copy was used.
    Backup,
    /// Neither file exists: a first run on built-in defaults.
    FirstRun,
    /// At least one file exists but neither loaded: running on built-in
    /// defaults. The unreadable files were preserved (see
    /// [`preserve_unreadable`]) before anything could save over them.
    Defaults,
}

/// Result of [`load_for_startup`].
#[derive(Debug)]
pub struct StartupLoad {
    pub config: AppConfig,
    pub source: ConfigSource,
    /// The primary file's load error when `source` is `Backup` or
    /// `Defaults`.
    pub error: Option<String>,
    /// Copies made of unreadable files (`*.corrupt-<unix-ms>`).
    pub preserved: Vec<PathBuf>,
}

/// Startup load: [`load_detailed`] plus the preservation the app shell
/// needs before anything can save (finding F2 in
/// `docs/code-review-v0.1.60.md`).
///
/// Every save rotates the current primary over `.bak`, and startup
/// itself saves (catalog seeding, setup reconciliation). So a shell that
/// silently fell back would destroy the evidence: the corrupt primary
/// becomes `.bak`, then is overwritten by the next save. This function
/// copies whatever could not be loaded aside first:
///
/// - `Backup`: the unreadable primary (if it exists) is preserved; the
///   `.bak` that loaded is not (it is the config now in memory).
/// - `Defaults`: both the primary and the `.bak` (whichever exist).
/// - `FirstRun`: nothing exists, nothing to preserve.
///
/// A failed copy is not fatal (the app still has to start); it is
/// reported in `error` alongside the load error.
pub fn load_for_startup(path: &Path) -> StartupLoad {
    let path_buf = path.to_path_buf();
    match load_detailed(&path_buf) {
        Ok(report) if !report.used_backup => StartupLoad {
            config: report.config,
            source: ConfigSource::Primary,
            error: None,
            preserved: Vec::new(),
        },
        Ok(report) => {
            let primary_err = load_one(&path_buf)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "primary config failed to load".to_string());
            let (preserved, copy_err) =
                preserve_unreadable(std::slice::from_ref(&path_buf), now_unix_ms());
            StartupLoad {
                config: report.config,
                source: ConfigSource::Backup,
                error: Some(join_errors(primary_err, copy_err)),
                preserved,
            }
        }
        Err(e) => {
            let bak = bak_path(path);
            if !path.exists() && !bak.exists() {
                return StartupLoad {
                    config: AppConfig::default_config(),
                    source: ConfigSource::FirstRun,
                    error: None,
                    preserved: Vec::new(),
                };
            }
            let (preserved, copy_err) = preserve_unreadable(&[path_buf, bak], now_unix_ms());
            StartupLoad {
                config: AppConfig::default_config(),
                source: ConfigSource::Defaults,
                error: Some(join_errors(e.to_string(), copy_err)),
                preserved,
            }
        }
    }
}

fn join_errors(load_err: String, copy_err: Option<String>) -> String {
    match copy_err {
        Some(c) => format!("{load_err}; {c}"),
        None => load_err,
    }
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Copy each existing file in `files` to `<file>.corrupt-<unix_ms>`
/// beside it (copy, not move: the original stays where a manual repair
/// expects it). Missing files are skipped. Returns the copies made and,
/// if any copy failed, a description of the failures.
pub fn preserve_unreadable(files: &[PathBuf], unix_ms: u128) -> (Vec<PathBuf>, Option<String>) {
    let mut made = Vec::new();
    let mut failures = Vec::new();
    for f in files {
        if !f.exists() {
            continue;
        }
        let mut dest = f.as_os_str().to_owned();
        dest.push(format!(".corrupt-{unix_ms}"));
        let dest = PathBuf::from(dest);
        match fs::copy(f, &dest) {
            Ok(_) => made.push(dest),
            Err(e) => failures.push(format!(
                "could not preserve {} as {}: {e}",
                f.display(),
                dest.display()
            )),
        }
    }
    let err = (!failures.is_empty()).then(|| failures.join("; "));
    (made, err)
}

/// Serializes [`save`] process-wide. Saves share fixed temp names
/// (`config.json.tmp`, `config.json.bak.tmp`), so two concurrent saves
/// could interleave the rotation and the renames (finding PS-2,
/// docs/code-review-v0.1.60.md); callers hold AppState's config lock
/// too, but not every writer goes through it (wizard steps, tools).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Validate and save the config.
///
/// The config is validated *before* anything is written, so a failed
/// save never clobbers the last-known-good copy. Order (finding PS-7,
/// docs/code-review-v0.1.60.md), chosen so `config.json` exists at
/// every instant and a crash or failure leaves either the old or the
/// new file in place, never neither:
///
/// 1. write the new JSON to `config.json.tmp` and fsync it;
/// 2. copy the current `config.json` to `.bak` (via a temp + rename);
/// 3. rename the temp over `config.json` (`fs::rename` replaces the
///    destination atomically on both Windows — `MoveFileExW` with
///    `MOVEFILE_REPLACE_EXISTING` — and POSIX).
///
/// The previous order (rotate the primary to `.bak` first, then write)
/// left no `config.json` at all whenever the write failed (disk full, a
/// locked temp file), and the remove-then-rename it used could destroy
/// `.bak` on a failed rename.
pub fn save(config: &AppConfig, path: &PathBuf) -> Result<(), ConfigError> {
    validate(config)?;
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(config)?;
    let tmp = tmp_path(path);
    if let Err(e) = write_synced(&tmp, json.as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    if path.exists() {
        let bak = bak_path(path);
        let bak_tmp = tmp_path(&bak);
        let rotated = fs::copy(path, &bak_tmp).and_then(|_| fs::rename(&bak_tmp, &bak));
        if let Err(e) = rotated {
            let _ = fs::remove_file(&bak_tmp);
            let _ = fs::remove_file(&tmp);
            return Err(ConfigError::BackupFailed(format!(
                "could not copy {} to {}: {e}",
                path.display(),
                bak.display()
            )));
        }
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Write `bytes` to `path` and flush them to stable storage, so the
/// rename that publishes the file can never expose a zero-length or
/// partial config after a power loss.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn load_one(path: &PathBuf) -> Result<AppConfig, ConfigError> {
    let data = fs::read(path)?;
    let raw: serde_json::Value = serde_json::from_slice(&data)?;
    let cfg = migrate(raw)?;
    validate(&cfg).map_err(|e| match e {
        ConfigError::Validation(msg) => {
            ConfigError::Validation(format!("{}: {msg}", path.display()))
        }
        other => other,
    })?;
    Ok(cfg)
}

fn bak_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;
    use crate::*;

    fn unique_tmpdir(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "manager-config-unit-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_validates_before_writing() {
        let dir = unique_tmpdir("save-invalid");
        let path = dir.join("config.json");
        let mut cfg = working_config();
        cfg.backends[0].port = cfg.backends[1].port; // invalid
        let err = save(&cfg, &path).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(!path.exists(), "invalid config must not be written");
    }

    #[test]
    fn load_missing_file_is_io_error() {
        let dir = unique_tmpdir("load-missing");
        let err = load(&dir.join("nope.json")).unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)), "unexpected: {err}");
    }
}

#[cfg(test)]
mod startup_load_tests {
    //! F1 / F2 regression tests (`docs/code-review-v0.1.60.md`).
    use super::*;
    use crate::*;

    fn scratch(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "manager-config-startup-{}-{}-{}",
            name,
            std::process::id(),
            now_unix_ms()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn corrupt_files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn default_config_pins_litellm() {
        let cfg = AppConfig::default_config();
        let c = cfg.updates.components.get(LITELLM_COMPONENT).unwrap();
        assert_eq!(c.pinned_version.as_deref(), Some(DEFAULT_LITELLM_PIN));
        assert!(!c.auto_update);
        validate(&cfg).unwrap();
    }

    #[test]
    fn migrate_backfills_litellm_pin_only_when_unpinned() {
        // Component absent entirely (file written before F1).
        let mut raw = serde_json::to_value(AppConfig::default_config()).unwrap();
        raw["updates"]["components"] = serde_json::json!({});
        let cfg = migrate(raw).unwrap();
        assert_eq!(
            cfg.updates.components[LITELLM_COMPONENT]
                .pinned_version
                .as_deref(),
            Some(DEFAULT_LITELLM_PIN)
        );

        // Component present but unpinned: pin set, other fields kept.
        let mut raw = serde_json::to_value(AppConfig::default_config()).unwrap();
        raw["updates"]["components"] = serde_json::json!({
            "litellm": {"pinned_version": null, "auto_update": true, "last_checked": null},
            "llamacpp": {"pinned_version": null, "auto_update": false, "last_checked": null}
        });
        let cfg = migrate(raw).unwrap();
        let l = &cfg.updates.components[LITELLM_COMPONENT];
        assert_eq!(l.pinned_version.as_deref(), Some(DEFAULT_LITELLM_PIN));
        assert!(l.auto_update, "only the pin is backfilled");
        // Other components are not pinned by the backfill.
        assert_eq!(cfg.updates.components["llamacpp"].pinned_version, None);

        // An explicit operator pin is never overwritten.
        let mut raw = serde_json::to_value(AppConfig::default_config()).unwrap();
        raw["updates"]["components"]["litellm"]["pinned_version"] = serde_json::json!("1.104.2");
        let cfg = migrate(raw).unwrap();
        assert_eq!(
            cfg.updates.components[LITELLM_COMPONENT]
                .pinned_version
                .as_deref(),
            Some("1.104.2")
        );
    }

    #[test]
    fn startup_first_run_preserves_nothing() {
        let dir = scratch("first-run");
        let r = load_for_startup(&dir.join("config.json"));
        assert_eq!(r.source, ConfigSource::FirstRun);
        assert!(r.error.is_none());
        assert!(r.preserved.is_empty());
        assert!(corrupt_files_in(&dir).is_empty());
    }

    #[test]
    fn startup_primary_ok_preserves_nothing() {
        let dir = scratch("primary");
        let path = dir.join("config.json");
        save(&AppConfig::default_config(), &path).unwrap();
        let r = load_for_startup(&path);
        assert_eq!(r.source, ConfigSource::Primary);
        assert!(r.preserved.is_empty());
        assert!(corrupt_files_in(&dir).is_empty());
    }

    #[test]
    fn startup_backup_fallback_preserves_corrupt_primary() {
        let dir = scratch("backup");
        let path = dir.join("config.json");
        save(&AppConfig::default_config(), &path).unwrap();
        save(&AppConfig::default_config(), &path).unwrap(); // creates .bak
        fs::write(&path, b"{ not json").unwrap();
        let r = load_for_startup(&path);
        assert_eq!(r.source, ConfigSource::Backup);
        assert!(r.error.is_some());
        assert_eq!(r.preserved.len(), 1);
        assert_eq!(fs::read(&r.preserved[0]).unwrap(), b"{ not json");
        // The original stays in place.
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
        // A later save (startup seeding) cannot destroy the evidence.
        save(&r.config, &path).unwrap();
        save(&r.config, &path).unwrap();
        assert_eq!(fs::read(&r.preserved[0]).unwrap(), b"{ not json");
    }

    #[test]
    fn startup_defaults_preserves_both_unreadable_files_before_any_save() {
        let dir = scratch("defaults");
        let path = dir.join("config.json");
        fs::write(&path, b"primary garbage").unwrap();
        fs::write(bak_path(&path), b"bak garbage").unwrap();
        let r = load_for_startup(&path);
        assert_eq!(r.source, ConfigSource::Defaults);
        assert!(r.error.is_some());
        assert_eq!(r.preserved.len(), 2);
        let names = corrupt_files_in(&dir);
        assert!(
            names.iter().any(|n| n.starts_with("config.json.corrupt-")),
            "{names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("config.json.bak.corrupt-")),
            "{names:?}"
        );
        // The defaults path then saves (catalog seeding) twice — the
        // rotation that used to overwrite both originals.
        save(&r.config, &path).unwrap();
        save(&r.config, &path).unwrap();
        let mut contents: Vec<Vec<u8>> = r.preserved.iter().map(|p| fs::read(p).unwrap()).collect();
        contents.sort();
        assert_eq!(
            contents,
            vec![b"bak garbage".to_vec(), b"primary garbage".to_vec()]
        );
    }

    #[test]
    fn startup_defaults_with_only_corrupt_bak() {
        let dir = scratch("only-bak");
        let path = dir.join("config.json");
        fs::write(bak_path(&path), b"bak garbage").unwrap();
        let r = load_for_startup(&path);
        assert_eq!(r.source, ConfigSource::Defaults);
        assert_eq!(r.preserved.len(), 1);
        assert_eq!(fs::read(&r.preserved[0]).unwrap(), b"bak garbage");
    }

    /// PS-7: a save that fails to write never removes the primary.
    #[test]
    fn failed_save_keeps_the_primary_and_bak() {
        let dir = scratch("save-fail");
        let path = dir.join("config.json");
        let mut cfg = AppConfig::default_config();
        cfg.optimize_purpose = "first".to_string();
        save(&cfg, &path).unwrap();
        cfg.optimize_purpose = "second".to_string();
        save(&cfg, &path).unwrap();
        let primary_before = fs::read(&path).unwrap();
        let bak_before = fs::read(bak_path(&path)).unwrap();
        // Make the temp write fail: the temp path is a directory.
        fs::create_dir_all(tmp_path(&path)).unwrap();
        cfg.optimize_purpose = "third".to_string();
        assert!(save(&cfg, &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), primary_before);
        assert_eq!(fs::read(bak_path(&path)).unwrap(), bak_before);
        fs::remove_dir_all(tmp_path(&path)).unwrap();
        // And a later save works and rotates normally.
        save(&cfg, &path).unwrap();
        assert_eq!(fs::read(bak_path(&path)).unwrap(), primary_before);
        assert_eq!(load(&path).unwrap().optimize_purpose, "third");
    }

    /// PS-2: concurrent saves from many threads never leave a torn or
    /// missing file; the result is one of the saved configs.
    #[test]
    fn concurrent_saves_leave_a_whole_file() {
        let dir = scratch("save-race");
        let path = dir.join("config.json");
        save(&AppConfig::default_config(), &path).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut cfg = AppConfig::default_config();
                    cfg.optimize_purpose = format!("writer-{i}");
                    for _ in 0..10 {
                        save(&cfg, &path).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let loaded = load_detailed(&path).unwrap();
        assert!(!loaded.used_backup);
        assert!(loaded.config.optimize_purpose.starts_with("writer-"));
    }

    #[test]
    fn preserve_unreadable_skips_missing_and_names_by_timestamp() {
        let dir = scratch("preserve");
        let a = dir.join("a.json");
        fs::write(&a, b"x").unwrap();
        let (made, err) = preserve_unreadable(&[a.clone(), dir.join("missing.json")], 1234);
        assert!(err.is_none());
        assert_eq!(made, vec![dir.join("a.json.corrupt-1234")]);
        assert_eq!(fs::read(&a).unwrap(), b"x", "copy, not move");
    }
}
