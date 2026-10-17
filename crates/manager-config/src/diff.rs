//! Structured backend-config diffs (spec §14: `backend_preview_diff` /
//! `backend_apply_config`).
//!
//! The UI sends a JSON `patch`; it deserializes into [`BackendPatch`]
//! (every field optional). [`diff_config`] renders the human-readable
//! field-level diff for the preview dialog; [`apply_config_diff`] applies the
//! patch to a backend, re-validates the whole config, and rolls the backend
//! back untouched if validation fails.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{validate, AppConfig, ConfigError, SplitMode};
use std::path::PathBuf;

/// Patchable backend fields. All optional; `None` = leave unchanged.
/// `raw_flags` is `Option<Option<String>>` so the UI can distinguish
/// "not patched" (`None`) from "clear the raw flags" (`Some(None)`).
///
/// Deliberately absent: `tool_call_probe`. The probe is runtime
/// state measured by the tool-call probe command (MCP design §7.2),
/// not operator configuration, so it is never patchable and never
/// rendered as a diff / activation change. Applying a patch that
/// changes `model_file` or `raw_flags` *resets* it instead (see
/// [`apply_config_diff`]).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BackendPatch {
    pub enabled: Option<bool>,
    pub model_file: Option<PathBuf>,
    pub port: Option<u16>,
    pub n_ctx: Option<u32>,
    pub n_batch: Option<u32>,
    pub tensor_split: Option<Vec<f32>>,
    pub split_mode: Option<SplitMode>,
    pub raw_flags: Option<Option<String>>,
    pub max_retries: Option<u32>,
    pub backoff_base_secs: Option<u64>,
}

/// One changed field: old value vs new value, JSON-rendered for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: String,
    pub old: Value,
    pub new: Value,
}

/// Preview of a backend config patch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiff {
    pub backend_id: String,
    /// Empty when the patch changes nothing.
    pub changes: Vec<FieldChange>,
    pub valid: bool,
}

fn changed(field: &str, old: Value, new: Value, out: &mut Vec<FieldChange>) {
    if old != new {
        out.push(FieldChange {
            field: field.to_string(),
            old,
            new,
        });
    }
}

/// Render the field-level diff between a backend's current config and a
/// patch, without applying anything.
pub fn diff_config(
    backend_id: &str,
    current: &super::BackendConfig,
    patch: &BackendPatch,
) -> ConfigDiff {
    let mut changes = Vec::new();
    if let Some(v) = patch.enabled {
        changed(
            "enabled",
            Value::from(current.enabled),
            Value::from(v),
            &mut changes,
        );
    }
    if let Some(ref v) = patch.model_file {
        changed(
            "model_file",
            Value::from(current.model_file.to_string_lossy().as_ref()),
            Value::from(v.to_string_lossy().as_ref()),
            &mut changes,
        );
    }
    if let Some(v) = patch.port {
        changed(
            "port",
            Value::from(current.port),
            Value::from(v),
            &mut changes,
        );
    }
    if let Some(v) = patch.n_ctx {
        changed(
            "n_ctx",
            Value::from(current.flags.n_ctx),
            Value::from(v),
            &mut changes,
        );
    }
    if let Some(v) = patch.n_batch {
        changed(
            "n_batch",
            Value::from(current.flags.n_batch),
            Value::from(v),
            &mut changes,
        );
    }
    if let Some(ref v) = patch.tensor_split {
        changed(
            "tensor_split",
            serde_json::to_value(&current.flags.tensor_split).unwrap_or(Value::Null),
            serde_json::to_value(v).unwrap_or(Value::Null),
            &mut changes,
        );
    }
    if let Some(v) = patch.split_mode {
        changed(
            "split_mode",
            serde_json::to_value(current.flags.split_mode).unwrap_or(Value::Null),
            serde_json::to_value(v).unwrap_or(Value::Null),
            &mut changes,
        );
    }
    if let Some(ref v) = patch.raw_flags {
        changed(
            "raw_flags",
            match &current.raw_flags {
                Some(s) => Value::from(s.as_str()),
                None => Value::Null,
            },
            match v {
                Some(s) => Value::from(s.as_str()),
                None => Value::Null,
            },
            &mut changes,
        );
    }
    if let Some(v) = patch.max_retries {
        changed(
            "max_retries",
            Value::from(current.restart_policy.max_retries),
            Value::from(v),
            &mut changes,
        );
    }
    if let Some(v) = patch.backoff_base_secs {
        changed(
            "backoff_base_secs",
            Value::from(current.restart_policy.backoff_base_secs),
            Value::from(v),
            &mut changes,
        );
    }
    ConfigDiff {
        backend_id: backend_id.to_string(),
        changes,
        valid: true,
    }
}

fn apply_patch_to_backend(backend: &mut super::BackendConfig, patch: &BackendPatch) {
    if let Some(v) = patch.enabled {
        backend.enabled = v;
    }
    if let Some(ref v) = patch.model_file {
        backend.model_file = v.clone();
    }
    if let Some(v) = patch.port {
        backend.port = v;
    }
    if let Some(v) = patch.n_ctx {
        backend.flags.n_ctx = v;
    }
    if let Some(v) = patch.n_batch {
        backend.flags.n_batch = v;
    }
    if let Some(ref v) = patch.tensor_split {
        backend.flags.tensor_split = v.clone();
    }
    if let Some(v) = patch.split_mode {
        backend.flags.split_mode = v;
    }
    if let Some(ref v) = patch.raw_flags {
        backend.raw_flags = v.clone();
    }
    if let Some(v) = patch.max_retries {
        backend.restart_policy.max_retries = v;
    }
    if let Some(v) = patch.backoff_base_secs {
        backend.restart_policy.backoff_base_secs = v;
    }
}

/// Apply a patch to a backend in `config`, returning the diff that was
/// applied.
///
/// The patch is applied to the live backend, then the whole config is
/// re-validated. If validation fails the backend is restored byte-for-byte
/// and the validation error is returned — the config is never left
/// half-patched.
pub fn apply_config_diff(
    config: &mut AppConfig,
    backend_id: &str,
    patch: &BackendPatch,
) -> Result<ConfigDiff, ConfigError> {
    let idx = config
        .backends
        .iter()
        .position(|b| b.id == backend_id)
        .ok_or_else(|| ConfigError::Validation(format!("unknown backend {backend_id:?}")))?;
    let diff = diff_config(backend_id, &config.backends[idx], patch);
    let original = config.backends[idx].clone();
    apply_patch_to_backend(&mut config.backends[idx], patch);
    // A model or raw-flags change invalidates the recorded tool-call
    // probe (MCP design §7.2): it measured a different serving
    // configuration. The reset is not itself a diff change — the
    // probe is runtime state, not operator config — and it rolls
    // back with the backend below if validation fails.
    if config.backends[idx].model_file != original.model_file
        || config.backends[idx].raw_flags != original.raw_flags
    {
        config.backends[idx].tool_call_probe = None;
    }
    match validate(config) {
        Ok(()) => Ok(diff),
        Err(e) => {
            config.backends[idx] = original;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppConfig;

    fn backend() -> super::super::BackendConfig {
        AppConfig::default_config().backends.remove(0)
    }

    #[test]
    fn diff_detects_only_changed_fields() {
        let b = backend();
        let patch = BackendPatch {
            n_ctx: Some(b.flags.n_ctx + 4096),
            port: Some(b.port),
            ..Default::default()
        };
        let diff = diff_config("planner", &b, &patch);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].field, "n_ctx");
        assert_eq!(diff.changes[0].old, Value::from(b.flags.n_ctx));
    }

    #[test]
    fn diff_empty_patch_has_no_changes() {
        let b = backend();
        let diff = diff_config("planner", &b, &BackendPatch::default());
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn apply_updates_and_validates() {
        let mut cfg = AppConfig::default_config();
        let id = cfg.backends[0].id.clone();
        let patch = BackendPatch {
            n_ctx: Some(16384),
            ..Default::default()
        };
        let diff = apply_config_diff(&mut cfg, &id, &patch).unwrap();
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(cfg.backends[0].flags.n_ctx, 16384);
    }

    #[test]
    fn apply_rejects_port_conflict_and_restores() {
        let mut cfg = AppConfig::default_config();
        let id0 = cfg.backends[0].id.clone();
        let port1 = cfg.backends[1].port;
        let before = cfg.backends[0].clone();
        let patch = BackendPatch {
            port: Some(port1),
            ..Default::default()
        };
        let err = apply_config_diff(&mut cfg, &id0, &patch).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert_eq!(cfg.backends[0].port, before.port);
        assert_eq!(cfg.backends[0].flags.n_ctx, before.flags.n_ctx);
    }

    #[test]
    fn apply_unknown_backend_errors() {
        let mut cfg = AppConfig::default_config();
        let err = apply_config_diff(&mut cfg, "nope", &BackendPatch::default()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn tool_call_probe_is_not_a_diff_field_and_resets_on_serving_change() {
        use crate::{ToolCallProbe, ToolCallProbeOutcome};
        let probe = Some(ToolCallProbe {
            outcome: ToolCallProbeOutcome::Verified,
            at_ms: 1_700_000_000_000,
        });
        // The probe never renders as a diff change: an empty patch
        // against a probed backend is an empty diff.
        let mut b = backend();
        b.tool_call_probe = probe;
        let diff = diff_config("planner", &b, &BackendPatch::default());
        assert!(diff.changes.is_empty());

        // A raw_flags change resets the probe to not measured, and
        // the diff reports only the operator's own change.
        let mut cfg = AppConfig::default_config();
        cfg.backends[0].tool_call_probe = probe;
        let id = cfg.backends[0].id.clone();
        let patch = BackendPatch {
            raw_flags: Some(Some("--threads 8".to_string())),
            ..Default::default()
        };
        let diff = apply_config_diff(&mut cfg, &id, &patch).unwrap();
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].field, "raw_flags");
        assert_eq!(cfg.backends[0].tool_call_probe, None);

        // A model_file change resets it too.
        let mut cfg = AppConfig::default_config();
        cfg.backends[0].tool_call_probe = probe;
        // Disable so validation does not require a registered model.
        cfg.backends[0].enabled = false;
        let id = cfg.backends[0].id.clone();
        let patch = BackendPatch {
            model_file: Some(PathBuf::from("models/other.gguf")),
            ..Default::default()
        };
        apply_config_diff(&mut cfg, &id, &patch).unwrap();
        assert_eq!(cfg.backends[0].tool_call_probe, None);

        // An unrelated change (n_ctx) leaves the probe in place.
        let mut cfg = AppConfig::default_config();
        cfg.backends[0].tool_call_probe = probe;
        let id = cfg.backends[0].id.clone();
        let patch = BackendPatch {
            n_ctx: Some(16384),
            ..Default::default()
        };
        apply_config_diff(&mut cfg, &id, &patch).unwrap();
        assert_eq!(cfg.backends[0].tool_call_probe, probe);
    }

    #[test]
    fn raw_flags_clear_is_distinguished_from_untouched() {
        let mut b = backend();
        b.raw_flags = Some("--verbose".to_string());
        // Some(None) clears.
        let patch = BackendPatch {
            raw_flags: Some(None),
            ..Default::default()
        };
        let diff = diff_config("x", &b, &patch);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].new, Value::Null);
        // None leaves untouched: no changes.
        let diff2 = diff_config("x", &b, &BackendPatch::default());
        assert!(diff2.changes.is_empty());
    }
}
