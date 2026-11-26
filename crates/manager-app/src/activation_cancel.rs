//! Which configuration fields activation cancel compares, restores,
//! and refuses to roll back (PS-10, docs/code-review-v0.1.60.md).
//!
//! Cancel restores the journal's snapshot — the configuration before
//! the first pending change — guarded by a comparison: the live
//! configuration must still equal the newest pending entry's
//! post-change state, or another path changed it since and a blind
//! restore would discard that change too. Comparing the WHOLE
//! configuration made cancel permanently refused as soon as the app
//! itself wrote a stamp in the background (a model's `verified_at`
//! after a download verified, `setup_completed` when the wizard
//! finished). This module classifies every top-level
//! [`AppConfig`] field so the comparison and the restore can treat
//! them differently:
//!
//! - **Bookkeeping** ([`BOOKKEEPING_FIELDS`], plus the per-model
//!   [`MODEL_BOOKKEEPING_FIELDS`] and per-backend
//!   [`BACKEND_BOOKKEEPING_FIELDS`]): state the app records about
//!   itself, never an operator's configuration decision. Stripped
//!   from both sides of the guard's comparison, and carried over
//!   from the CURRENT configuration into the restored one, so a
//!   newer stamp is never reverted to the snapshot's older value.
//! - **Security** ([`SECURITY_FIELDS`]): sections whose rollback to an
//!   older value could weaken the machine's posture. They are
//!   compared like any other field, and in addition cancel is
//!   REFUSED when one of them differs between the snapshot and the
//!   current configuration — restoring would roll it back.
//! - **Compared** ([`COMPARED_FIELDS`]): everything else, the
//!   operator's configuration. Compared and restored.
//!
//! The comparison is a DENYLIST: only the listed bookkeeping fields
//! are stripped, so a field added to `AppConfig` later is compared
//! (and restored) by default — the fail-safe direction, a spurious
//! refusal rather than a silently discarded change. The
//! `every_app_config_field_is_classified` test fails until a new
//! top-level field is placed in exactly one list here.
//!
//! App self-update check stamps (`last_checked_unix_ms` and
//! friends) are NOT configuration: they live in the app-update
//! state file beside it, so they need no entry here.

use manager_config::AppConfig;
use serde_json::Value;

/// Top-level bookkeeping fields: written by the app in the
/// background, never an operator configuration decision.
pub const BOOKKEEPING_FIELDS: &[&str] = &[
    // Set by the wizard-completion watcher when every step is done.
    "setup_completed",
    // Startup catalog seeding / repair markers (`ensure_catalog_models`):
    // one-time-migration flags, not settings. (The models the seeding
    // appends are compared normally under `models`.)
    "catalog_seeded",
    "defaults_repaired",
    "catalog_revision",
    "mcp_curated_seeded",
];

/// Per-model bookkeeping (`models[]`, matched by `id`): the
/// verification stamp the download watcher, `verify_model`, the
/// launch-time audit and the wizard's download step write.
pub const MODEL_BOOKKEEPING_FIELDS: &[&str] = &["verified_at", "verified_sha256"];

/// Per-backend bookkeeping (`backends[]`, matched by `id`): the
/// tool-call probe result the probe op records (MCP design §7.2).
pub const BACKEND_BOOKKEEPING_FIELDS: &[&str] = &["tool_call_probe"];

/// Security-relevant sections: cancel refuses rather than roll any
/// of them back to the snapshot's older value.
pub const SECURITY_FIELDS: &[&str] = &[
    // MXC execution policy (path and enforcement mode).
    "mxc",
    // Admin REST API enablement, port, bind/TLS settings. (API users
    // and tokens live in admin-users.json, never in the config.)
    "admin_api",
    // MCP server registry: exposure (gateway/local/disabled), launch
    // commands, credential references, allowed tools.
    "mcp",
    // Log streaming destination and endpoint policy: where redacted
    // logs leave the machine.
    "log_streaming",
];

/// Operator configuration that cancel compares and restores.
/// (`gateway` — including `bind_host` — is here, not under security:
/// its changes are exactly what the activation journal records, and
/// cancelling them is the feature.)
pub const COMPARED_FIELDS: &[&str] = &[
    "version",
    "data_dir",
    "view_modes",
    "backends",
    "gateway",
    "models",
    "telemetry",
    "winml",
    "postgres",
    "supervision",
    "updates",
    "app_update",
    "notifications",
    "optimize_purpose",
    // Tiered model storage tiers and policy (docs/tiered-model-storage.md).
    "model_storage",
];

/// A configuration in its canonical JSON form: parsed into
/// [`AppConfig`] and serialized back when possible, so journal
/// values written by an older build (missing serde-defaulted fields)
/// compare equal to the same configuration serialized today. A value
/// that does not parse is returned unchanged.
pub fn canonical(value: &Value) -> Value {
    serde_json::from_value::<AppConfig>(value.clone())
        .ok()
        .and_then(|cfg| serde_json::to_value(cfg).ok())
        .unwrap_or_else(|| value.clone())
}

/// `value` (a serialized [`AppConfig`]) with every bookkeeping field
/// removed — the form the cancel guard compares.
pub fn strip_bookkeeping(value: &Value) -> Value {
    let mut v = value.clone();
    if let Some(obj) = v.as_object_mut() {
        for field in BOOKKEEPING_FIELDS {
            obj.remove(*field);
        }
        strip_items(obj.get_mut("models"), MODEL_BOOKKEEPING_FIELDS);
        strip_items(obj.get_mut("backends"), BACKEND_BOOKKEEPING_FIELDS);
    }
    v
}

fn strip_items(list: Option<&mut Value>, fields: &[&str]) {
    let Some(Value::Array(items)) = list else {
        return;
    };
    for item in items.iter_mut() {
        if let Some(obj) = item.as_object_mut() {
            for field in fields {
                obj.remove(*field);
            }
        }
    }
}

/// Canonical form with bookkeeping stripped: the comparison key.
pub fn normalized(value: &Value) -> Value {
    strip_bookkeeping(&canonical(value))
}

/// Whether two serialized configurations are equal apart from
/// bookkeeping.
pub fn same_configuration(a: &Value, b: &Value) -> bool {
    normalized(a) == normalized(b)
}

/// The security sections that differ between the snapshot and the
/// current configuration — those a restore would roll back.
pub fn changed_security_sections(snapshot: &Value, current: &Value) -> Vec<&'static str> {
    let (snapshot, current) = (canonical(snapshot), canonical(current));
    SECURITY_FIELDS
        .iter()
        .copied()
        .filter(|f| snapshot.get(*f) != current.get(*f))
        .collect()
}

/// The configuration a cancel installs: the snapshot, with every
/// bookkeeping field taken from `current` (a newer `verified_at`
/// stamp, `setup_completed`, a fresher tool-call probe are kept, not
/// reverted). Per-item bookkeeping is matched by `id`; an item the
/// snapshot has and the current configuration does not keeps the
/// snapshot's own values. Tool-call probes carried over onto a
/// backend whose model file or raw flags the restore changes are
/// reset — they described the other launch (MCP design §7.2).
pub fn restored_config(snapshot: &Value, current: &AppConfig) -> Result<AppConfig, String> {
    let current_value = serde_json::to_value(current)
        .map_err(|e| format!("current configuration is not serializable: {e}"))?;
    let mut restored = canonical(snapshot);
    let Some(obj) = restored.as_object_mut() else {
        return Err("the saved snapshot is not a configuration object".to_string());
    };
    for field in BOOKKEEPING_FIELDS {
        match current_value.get(*field) {
            Some(v) => {
                obj.insert((*field).to_string(), v.clone());
            }
            None => {
                obj.remove(*field);
            }
        }
    }
    carry_items(
        obj.get_mut("models"),
        current_value.get("models"),
        MODEL_BOOKKEEPING_FIELDS,
    );
    carry_items(
        obj.get_mut("backends"),
        current_value.get("backends"),
        BACKEND_BOOKKEEPING_FIELDS,
    );
    let mut cfg: AppConfig = serde_json::from_value(restored)
        .map_err(|e| format!("the saved snapshot cannot be restored: {e}"))?;
    manager_config::reset_stale_tool_call_probes(current, &mut cfg);
    Ok(cfg)
}

fn carry_items(target: Option<&mut Value>, source: Option<&Value>, fields: &[&str]) {
    let (Some(Value::Array(items)), Some(Value::Array(sources))) = (target, source) else {
        return;
    };
    for item in items.iter_mut() {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let Some(id) = obj.get("id").cloned() else {
            continue;
        };
        let Some(src) = sources
            .iter()
            .find(|s| s.get("id") == Some(&id))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for field in fields {
            match src.get(*field) {
                Some(v) => {
                    obj.insert((*field).to_string(), v.clone());
                }
                None => {
                    obj.remove(*field);
                }
            }
        }
    }
}

/// The default configuration plus one registered model (the default
/// carries none), for tests that exercise per-model bookkeeping.
#[cfg(test)]
pub(crate) fn test_config_with_model() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.models.push(manager_config::ModelConfig {
        id: "test-model".to_string(),
        gguf_path: std::path::PathBuf::from("models/test.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 8.0,
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
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_app_config_field_is_classified() {
        // Fails when AppConfig gains a top-level field that no list
        // here classifies: decide whether cancel compares it,
        // carries it over as bookkeeping, or guards it as security.
        let value = serde_json::to_value(AppConfig::default_config()).unwrap();
        let keys: Vec<&String> = value.as_object().expect("an object").keys().collect();
        for key in &keys {
            let hits = [COMPARED_FIELDS, BOOKKEEPING_FIELDS, SECURITY_FIELDS]
                .iter()
                .filter(|list| list.contains(&key.as_str()))
                .count();
            assert_eq!(
                hits, 1,
                "AppConfig field `{key}` must be classified in exactly one of \
                 COMPARED_FIELDS / BOOKKEEPING_FIELDS / SECURITY_FIELDS (activation_cancel.rs)"
            );
        }
        // And no list names a field that no longer exists.
        for name in COMPARED_FIELDS
            .iter()
            .chain(BOOKKEEPING_FIELDS)
            .chain(SECURITY_FIELDS)
        {
            assert!(
                keys.iter().any(|k| k.as_str() == *name),
                "classified field `{name}` is not an AppConfig field"
            );
        }
    }

    #[test]
    fn nested_bookkeeping_fields_exist() {
        // The per-item lists must name real fields, or stripping
        // silently does nothing.
        let mut cfg = test_config_with_model();
        let m = cfg.models.first_mut().expect("a model");
        m.verified_at = Some(chrono::Utc::now());
        m.verified_sha256 = Some("ab".repeat(32));
        let b = cfg.backends.first_mut().expect("default backends");
        b.tool_call_probe = Some(manager_config::ToolCallProbe {
            outcome: manager_config::ToolCallProbeOutcome::Verified,
            at_ms: 1,
        });
        let v = serde_json::to_value(&cfg).unwrap();
        for f in MODEL_BOOKKEEPING_FIELDS {
            assert!(v["models"][0].get(*f).is_some(), "models[].{f}");
        }
        for f in BACKEND_BOOKKEEPING_FIELDS {
            assert!(v["backends"][0].get(*f).is_some(), "backends[].{f}");
        }
    }

    #[test]
    fn bookkeeping_differences_compare_equal_and_others_do_not() {
        let a = test_config_with_model();
        let mut b = a.clone();
        b.setup_completed = !a.setup_completed;
        b.models[0].verified_at = Some(chrono::Utc::now());
        b.models[0].verified_sha256 = Some("cd".repeat(32));
        let av = serde_json::to_value(&a).unwrap();
        let bv = serde_json::to_value(&b).unwrap();
        assert!(same_configuration(&av, &bv));
        b.gateway.port += 1;
        let bv = serde_json::to_value(&b).unwrap();
        assert!(!same_configuration(&av, &bv));
    }

    #[test]
    fn restore_keeps_current_bookkeeping_and_resets_stale_probes() {
        let snapshot_cfg = test_config_with_model();
        let mut current = snapshot_cfg.clone();
        current.gateway.port += 1;
        current.setup_completed = true;
        let stamp = chrono::Utc::now();
        current.models[0].verified_at = Some(stamp);
        current.models[0].verified_sha256 = Some("ef".repeat(32));
        let probe = manager_config::ToolCallProbe {
            outcome: manager_config::ToolCallProbeOutcome::Verified,
            at_ms: 7,
        };
        current.backends[0].tool_call_probe = Some(probe);
        current.backends[1].tool_call_probe = Some(probe);
        // The restore changes backend 1's raw flags back: its probe
        // described the current launch, so it must not survive.
        current.backends[1].raw_flags = Some("--changed-since".to_string());
        let snapshot = serde_json::to_value(&snapshot_cfg).unwrap();
        let restored = restored_config(&snapshot, &current).unwrap();
        assert_eq!(restored.gateway.port, snapshot_cfg.gateway.port);
        assert!(restored.setup_completed);
        assert_eq!(restored.models[0].verified_at, Some(stamp));
        assert_eq!(
            restored.models[0].verified_sha256.as_deref(),
            Some("ef".repeat(32).as_str())
        );
        assert!(restored.backends[0].tool_call_probe.is_some());
        assert!(restored.backends[1].tool_call_probe.is_none());
        assert_eq!(
            restored.backends[1].raw_flags,
            snapshot_cfg.backends[1].raw_flags
        );
    }

    #[test]
    fn security_sections_that_differ_are_reported() {
        let snapshot_cfg = AppConfig::default_config();
        let mut current = snapshot_cfg.clone();
        assert!(changed_security_sections(
            &serde_json::to_value(&snapshot_cfg).unwrap(),
            &serde_json::to_value(&current).unwrap()
        )
        .is_empty());
        current.admin_api.port = current.admin_api.port.wrapping_add(1);
        assert_eq!(
            changed_security_sections(
                &serde_json::to_value(&snapshot_cfg).unwrap(),
                &serde_json::to_value(&current).unwrap()
            ),
            vec!["admin_api"]
        );
    }
}
