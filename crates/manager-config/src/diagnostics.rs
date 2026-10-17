//! Diagnostics bundle export (spec §14: `diagnostics_export_bundle`).
//!
//! Assembles a zip containing a config snapshot, per-scope recent logs,
//! a telemetry snapshot, and a versions file. The caller (Tauri layer)
//! supplies the log lines and telemetry JSON as plain data so this crate
//! does not depend on the supervisor or telemetry crates.

use std::io::Write;
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;

use super::{AppConfig, ConfigError};

/// Write `diagnostics-<unix_ms>.zip` into `dest_dir` and return its path.
///
/// Contents:
/// - `config.json` — pretty-printed [`AppConfig`] snapshot
/// - `logs/<scope>.log` — one file per scope, newest-first lines as given
/// - `telemetry.json` — caller-supplied snapshot JSON (verbatim)
/// - `versions.txt` — `name: version` lines
pub fn export_diagnostics(
    config: &AppConfig,
    log_scopes: &[(String, Vec<String>)],
    telemetry_snapshot_json: &str,
    versions: &[(String, String)],
    dest_dir: &Path,
) -> Result<PathBuf, ConfigError> {
    std::fs::create_dir_all(dest_dir)?;
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dest_dir.join(format!("diagnostics-{ts_ms}.zip"));
    let file = std::fs::File::create(&path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let config_json = serde_json::to_string_pretty(config).map_err(ConfigError::Json)?;
    zip.start_file("config.json", opts)
        .map_err(|e| ConfigError::Zip(e.to_string()))?;
    zip.write_all(config_json.as_bytes())
        .map_err(|e| ConfigError::Zip(e.to_string()))?;

    for (scope, lines) in log_scopes {
        let safe_scope: String = scope
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        zip.start_file(format!("logs/{safe_scope}.log"), opts)
            .map_err(|e| ConfigError::Zip(e.to_string()))?;
        for line in lines {
            zip.write_all(line.as_bytes())
                .map_err(|e| ConfigError::Zip(e.to_string()))?;
            zip.write_all(b"\n")
                .map_err(|e| ConfigError::Zip(e.to_string()))?;
        }
    }

    zip.start_file("telemetry.json", opts)
        .map_err(|e| ConfigError::Zip(e.to_string()))?;
    zip.write_all(telemetry_snapshot_json.as_bytes())
        .map_err(|e| ConfigError::Zip(e.to_string()))?;

    let mut versions_txt = String::new();
    for (name, version) in versions {
        versions_txt.push_str(&format!("{name}: {version}\n"));
    }
    zip.start_file("versions.txt", opts)
        .map_err(|e| ConfigError::Zip(e.to_string()))?;
    zip.write_all(versions_txt.as_bytes())
        .map_err(|e| ConfigError::Zip(e.to_string()))?;

    zip.finish().map_err(|e| ConfigError::Zip(e.to_string()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_contains_expected_entries() {
        let dir = std::env::temp_dir().join(format!(
            "llm-diag-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cfg = AppConfig::default_config();
        let logs = vec![(
            "planner".to_string(),
            vec!["line one".to_string(), "line two".to_string()],
        )];
        let path = export_diagnostics(
            &cfg,
            &logs,
            r#"{"gpus":[]}"#,
            &[("app".to_string(), "0.1.0".to_string())],
            &dir,
        )
        .unwrap();
        assert!(path.exists());

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"config.json".to_string()), "{names:?}");
        assert!(names.contains(&"logs/planner.log".to_string()), "{names:?}");
        assert!(names.contains(&"telemetry.json".to_string()), "{names:?}");
        assert!(names.contains(&"versions.txt".to_string()), "{names:?}");

        // config.json round-trips to an AppConfig.
        let mut cfg_file = archive.by_name("config.json").unwrap();
        let mut buf = String::new();
        use std::io::Read;
        cfg_file.read_to_string(&mut buf).unwrap();
        let parsed: AppConfig = serde_json::from_str(&buf).unwrap();
        assert_eq!(parsed.version, crate::CURRENT_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_scope_names_are_sanitized() {
        let dir = std::env::temp_dir().join(format!(
            "llm-diag-test2-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cfg = AppConfig::default_config();
        let logs = vec![("../evil".to_string(), vec!["x".to_string()])];
        let path = export_diagnostics(&cfg, &logs, "{}", &[], &dir).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains("..")),
            "path traversal in bundle: {names:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
