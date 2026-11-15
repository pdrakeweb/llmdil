//! Schema migration: defaults overlay, version stepping, and the
//! LiteLLM pin backfill.

use crate::{
    curated_mcp_servers, AppConfig, ComponentUpdate, ConfigError, CURRENT_VERSION,
    DEFAULT_LITELLM_PIN, LITELLM_COMPONENT,
};

/// Migrate a raw JSON config value forward to [`CURRENT_VERSION`].
///
/// Version detection: the `version` key, defaulting to 0 when absent (v0 =
/// pre-versioned configs). v0 → v1 overlays the stored values onto current
/// defaults so newly added fields get sane values, then stamps `version: 1`.
/// Unknown fields are ignored. A version newer than [`CURRENT_VERSION`] or a
/// non-object root is a [`ConfigError::MigrationFailed`].
pub fn migrate(raw: serde_json::Value) -> Result<AppConfig, ConfigError> {
    if !raw.is_object() {
        return Err(ConfigError::MigrationFailed(
            "config root must be a JSON object".to_string(),
        ));
    }
    let version = raw.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if version > CURRENT_VERSION {
        return Err(ConfigError::MigrationFailed(format!(
            "config version {version} is newer than supported version {CURRENT_VERSION}"
        )));
    }
    // MCP registry one-time migration (design §10.1), via the raw-JSON
    // route: capture an explicit `mcp` section before defaults are
    // overlaid. `apply_defaults` deep-merges over `default_config()`
    // (whose `mcp` is the seed), so a config with NO `mcp` key gets the
    // seeded registry injected — exactly once, at load; the seeded
    // registry is then written back on the next save, so the key
    // exists from then on. An explicit section, even an empty
    // `{"servers": []}`, is restored verbatim after the merge, so an
    // operator who later deletes every server is NOT resurrected on
    // the next load. (A `null` mcp is treated as absent.)
    let explicit_mcp = raw
        .as_object()
        .and_then(|o| o.get("mcp"))
        .filter(|v| !v.is_null())
        .cloned();
    // Curated-seeding marker (Phase 3, design §3.3): whether the raw
    // file carries `mcp_curated_seeded` at all. The defaults overlay
    // below fills a missing key from `default_config()` — where the
    // marker is born set — so presence in the RAW JSON is the only
    // honest signal that the one-time append already ran for this
    // file. A pre-Phase-3 file has no marker key; a Phase 3 file
    // always does (the field serializes unconditionally).
    let mcp_curated_marker_present = raw
        .as_object()
        .map(|o| o.contains_key("mcp_curated_seeded"))
        .unwrap_or(false);
    let mut merged = raw;
    apply_defaults(&mut merged)?;
    if let Some(mcp) = explicit_mcp {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("mcp".to_string(), mcp);
        }
    }
    if !mcp_curated_marker_present {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("mcp_curated_seeded".to_string(), serde_json::json!(false));
        }
    }
    if version != CURRENT_VERSION {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("version".to_string(), serde_json::json!(CURRENT_VERSION));
        }
    }
    let mut cfg: AppConfig = serde_json::from_value(merged).map_err(|e| {
        ConfigError::MigrationFailed(format!("migrated config failed schema check: {e}"))
    })?;
    // One-time curated append (Phase 3): a configuration that has
    // never been through this migration gains any curated server it
    // lacks (by id — an operator-renamed or edited entry with a
    // curated id is left alone), exactly once: the marker is set on
    // the returned config and persists on the next save, so curated
    // entries the operator later deletes stay deleted. This composes
    // with the verbatim explicit-`mcp` restore above: the restore
    // decides what the file said; this append is the migration the
    // design orders for pre-Phase-3 files.
    if !cfg.mcp_curated_seeded {
        for server in curated_mcp_servers() {
            if !cfg.mcp.servers.iter().any(|s| s.id == server.id) {
                cfg.mcp.servers.push(server);
            }
        }
        cfg.mcp_curated_seeded = true;
    }
    backfill_litellm_pin(&mut cfg);
    Ok(cfg)
}

/// Load-time backfill for [`DEFAULT_LITELLM_PIN`] (finding F1).
///
/// Sets the pin only when the litellm component is absent or carries no
/// `pinned_version`; an operator's explicit pin (older or newer) is left
/// untouched, as are the component's other fields. Returns whether the
/// config changed. The change reaches disk on the next save; until then
/// the in-memory config (which the wizard reads) already carries it.
pub fn backfill_litellm_pin(cfg: &mut AppConfig) -> bool {
    let entry = cfg
        .updates
        .components
        .entry(LITELLM_COMPONENT.to_string())
        .or_insert_with(|| ComponentUpdate {
            pinned_version: None,
            auto_update: false,
            last_checked: None,
        });
    let unpinned = entry
        .pinned_version
        .as_deref()
        .map(|v| v.trim().is_empty())
        .unwrap_or(true);
    if unpinned {
        entry.pinned_version = Some(DEFAULT_LITELLM_PIN.to_string());
    }
    unpinned
}

/// Deep-merge `raw` over the current defaults: stored values win, missing
/// keys are filled from [`AppConfig::default_config`].
fn apply_defaults(raw: &mut serde_json::Value) -> Result<(), ConfigError> {
    let defaults = serde_json::to_value(AppConfig::default_config()).map_err(|e| {
        ConfigError::MigrationFailed(format!("default config failed to serialize: {e}"))
    })?;
    let merged = deep_merge(defaults, raw.take());
    *raw = merged;
    Ok(())
}

fn deep_merge(base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    match (base, over) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => deep_merge(bv, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            serde_json::Value::Object(b)
        }
        // Arrays and scalars: the stored value wins wholesale.
        (_, over) => over,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;
    use crate::*;

    #[test]
    fn migrate_v0_without_version_key() {
        // v0: no "version" key, partial fields.
        let raw = serde_json::json!({
            "data_dir": "/tmp/llm-test",
            "gateway": { "port": 4000 },
            "telemetry": { "poll_interval_ms": 500 }
        });
        let cfg = migrate(raw).expect("v0 must migrate");
        assert_eq!(cfg.version, CURRENT_VERSION);
        assert_eq!(cfg.gateway.port, 4000);
        // New-in-v1 fields filled from defaults.
        assert_eq!(cfg.gateway.request_log_len, 100);
        assert_eq!(cfg.telemetry.poll_interval_ms, 500);
        assert_eq!(cfg.telemetry.retention_secs, 60);
        assert_eq!(cfg.gateway.groups.len(), 4);
        validate(&cfg).expect("migrated config must validate");
    }

    #[test]
    fn migrate_current_version_passthrough() {
        let raw = serde_json::to_value(working_config()).unwrap();
        let cfg = migrate(raw).expect("v1 must pass through");
        assert_eq!(cfg.version, CURRENT_VERSION);
        assert_eq!(cfg.backends.len(), 4);
    }

    #[test]
    fn migrate_future_version_rejected() {
        let raw = serde_json::json!({ "version": CURRENT_VERSION + 1 });
        let err = migrate(raw).unwrap_err().to_string();
        assert!(err.contains("newer than supported"), "unexpected: {err}");
    }

    #[test]
    fn migrate_non_object_rejected() {
        let err = migrate(serde_json::json!([1, 2, 3])).unwrap_err();
        assert!(matches!(err, ConfigError::MigrationFailed(_)));
    }

    #[test]
    fn migrate_ignores_unknown_fields() {
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw["some_future_field"] = serde_json::json!({ "nested": true });
        raw["backends"][0]["another_future_field"] = serde_json::json!(42);
        let cfg = migrate(raw).expect("unknown fields must be ignored");
        assert_eq!(cfg.version, CURRENT_VERSION);
        validate(&cfg).expect("must still validate");
    }
}
