//! Curated-catalog maintenance: repairing stale seeded entries,
//! appending missing catalog models, refreshing catalog definitions,
//! and resetting stale tool-call probes.

use std::path::PathBuf;

use crate::{AppConfig, ModelConfig, SplitMode};

/// One curated catalog entry's repair data for
/// [`repair_catalog_defaults`]: the values a configuration seeded by an
/// older version may be missing or hold as placeholders.
pub struct CatalogModelDefaults {
    pub model_id: String,
    pub sha256: String,
    pub params_b: f32,
    /// The default backend this model belongs to.
    pub backend_id: String,
}

/// One-time repair for configurations written before the catalog
/// carried real values. For each catalog model present in the config:
///
/// - a `sha256` still equal to `placeholder_sha` is replaced with the
///   catalog's real pinned hash (a deliberately emptied or otherwise
///   edited hash is left alone);
/// - a non-positive `params_b` is set from the catalog (validation
///   requires a positive value, and older seeding wrote 0);
/// - the model's default backend is recorded in `assigned_backends`
///   when missing.
///
/// For each catalog backend: a `model_file` still equal to the
/// untouched default (`models/<backend-id>.gguf`) is aligned to its
/// model's registered path, and the backend is enabled (older
/// defaults shipped the fleet disabled; enabling is what the current
/// defaults do — a backend whose model is not downloaded and verified
/// still fails its start preflight). Returns true when anything
/// changed. The caller gates this on [`AppConfig::defaults_repaired`]
/// so it runs exactly once and never overrides later operator choices.
pub fn repair_catalog_defaults(
    cfg: &mut AppConfig,
    catalog: &[CatalogModelDefaults],
    placeholder_sha: &str,
) -> bool {
    let mut changed = false;
    for entry in catalog {
        let Some(model) = cfg.models.iter_mut().find(|m| m.id == entry.model_id) else {
            continue;
        };
        if model.sha256 == placeholder_sha {
            model.sha256 = entry.sha256.clone();
            changed = true;
        }
        if model.params_b <= 0.0 {
            model.params_b = entry.params_b;
            changed = true;
        }
        if !model.assigned_backends.contains(&entry.backend_id) {
            model.assigned_backends.push(entry.backend_id.clone());
            changed = true;
        }
        let gguf_path = model.gguf_path.clone();
        if let Some(backend) = cfg.backends.iter_mut().find(|b| b.id == entry.backend_id) {
            if backend.model_file == format!("models/{}.gguf", entry.backend_id) {
                backend.model_file = gguf_path;
                changed = true;
            }
            if !backend.enabled {
                backend.enabled = true;
                changed = true;
            }
        }
    }
    changed
}

/// One catalog entry for [`append_missing_catalog_models`]: a model
/// a configuration may predate.
#[derive(Debug, Clone)]
pub struct CatalogModelEntry {
    pub model_id: String,
    pub gguf_file_name: String,
    pub quant: String,
    pub params_b: f32,
    pub sha256: String,
    pub source_url: String,
    pub assigned_backends: Vec<String>,
    pub enabled: bool,
}

/// Append catalog models the configuration does not have yet (by
/// id). Used when the curated catalog gains entries after a
/// configuration was seeded: the new models arrive with their
/// catalog defaults (opt-in candidates disabled and unassigned)
/// instead of never reaching installed systems. Existing entries —
/// including operator-edited ones — are untouched. The caller
/// gates this on [`AppConfig::catalog_revision`] so it runs once
/// per catalog revision. Returns true when anything was appended.
pub fn append_missing_catalog_models(cfg: &mut AppConfig, entries: &[CatalogModelEntry]) -> bool {
    let mut changed = false;
    for entry in entries {
        if cfg.models.iter().any(|m| m.id == entry.model_id) {
            continue;
        }
        cfg.models.push(ModelConfig {
            id: entry.model_id.clone(),
            gguf_path: PathBuf::from("models").join(&entry.gguf_file_name),
            quant: entry.quant.clone(),
            params_b: entry.params_b,
            sha256: entry.sha256.clone(),
            verified_at: None,
            verified_sha256: None,
            source_url: entry.source_url.clone(),
            assigned_backends: entry.assigned_backends.clone(),
            enabled: entry.enabled,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        changed = true;
    }
    changed
}

/// One catalog definition replacement for
/// [`refresh_catalog_definition`]: the catalog's current entry for a
/// default-fleet model, the backend it serves, and the serving
/// change that rides the replacement.
pub struct CatalogDefinitionRefresh {
    pub entry: CatalogModelEntry,
    pub backend_id: String,
    /// Clear the backend's tensor split: the replacement model is
    /// served unsplit (planner) or by expert offload (coder), not
    /// by the split the previous definition needed.
    pub clear_split: bool,
    /// Raw llama-server flags the replacement model requires (for
    /// the coder, expert offload: `-ot exps=CPU`). Applied only
    /// when the backend has no raw flags of its own.
    pub raw_flags: Option<String>,
}

/// Apply a catalog definition replacement to an existing
/// configuration (catalog revision 2 replaced the planner and coder
/// models). When the stored pin differs from the catalog's, the
/// model entry takes the catalog's fields and its verification is
/// cleared — the replacement file has not been downloaded — and a
/// default backend still pointing at the model's previous file is
/// realigned to the new one. The riding serving change applies in
/// the same case: the backend's tensor split is cleared when
/// requested, and the required raw flags are set when the backend
/// has none. A backend the operator reassigned to another model,
/// or gave raw flags, keeps them. The caller gates this on
/// [`AppConfig::catalog_revision`] so it runs once per catalog
/// revision. Returns true when the definition was replaced.
pub fn refresh_catalog_definition(cfg: &mut AppConfig, refresh: &CatalogDefinitionRefresh) -> bool {
    let entry = &refresh.entry;
    let Some(model) = cfg.models.iter_mut().find(|m| m.id == entry.model_id) else {
        return false;
    };
    if model.sha256 == entry.sha256 {
        return false;
    }
    let old_path = model.gguf_path.clone();
    model.gguf_path = PathBuf::from("models").join(&entry.gguf_file_name);
    model.quant = entry.quant.clone();
    model.params_b = entry.params_b;
    model.sha256 = entry.sha256.clone();
    model.source_url = entry.source_url.clone();
    model.verified_at = None;
    model.verified_sha256 = None;
    let new_path = model.gguf_path.clone();
    if let Some(backend) = cfg.backends.iter_mut().find(|b| b.id == refresh.backend_id) {
        // The serving change applies only to a backend still
        // serving this model; a backend the operator reassigned to
        // another model's file keeps its file, split, and flags.
        let serving = backend.model_file == old_path;
        if serving {
            backend.model_file = new_path;
            // Advanced flags mode (tracker #60): the backend's
            // flags are operator-owned text. The refresh still
            // realigns the model file (identity, not flags), but
            // it must not clear the split or fill raw flags behind
            // the editor's back — the one-writer rule the command
            // layer enforces for the structured setters applies to
            // this load-time migration too.
            if !backend.flags_advanced {
                if refresh.clear_split
                    && (!backend.flags.tensor_split.is_empty()
                        || backend.flags.split_mode != SplitMode::None)
                {
                    backend.flags.tensor_split = Vec::new();
                    backend.flags.split_mode = SplitMode::None;
                }
                if let Some(raw) = &refresh.raw_flags {
                    if backend.raw_flags.is_none() {
                        backend.raw_flags = Some(raw.clone());
                    }
                }
            }
        }
    }
    true
}

/// Reset tool-call probes that a serving change has made stale
/// (MCP design §7.2): for every backend in `current` whose
/// `model_file` or `raw_flags` differs from the same-id backend in
/// `previous`, the recorded probe is cleared to `None` (not
/// measured) — it described a different model or different launch
/// flags. Backends new in `current` keep whatever probe they carry
/// (a new backend is constructed with `None`). Returns true when
/// any probe was reset.
pub fn reset_stale_tool_call_probes(previous: &AppConfig, current: &mut AppConfig) -> bool {
    let mut changed = false;
    for backend in current.backends.iter_mut() {
        if backend.tool_call_probe.is_none() {
            continue;
        }
        let Some(old) = previous.backends.iter().find(|b| b.id == backend.id) else {
            continue;
        };
        if old.model_file != backend.model_file || old.raw_flags != backend.raw_flags {
            backend.tool_call_probe = None;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::*;

    #[test]
    fn catalog_refresh_skips_flag_writes_for_advanced_backends() {
        // The load-time catalog refresh realigns the model file
        // but must not clear the split or fill raw flags of a
        // backend in advanced flags mode (tracker #60).
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-27b-planner".to_string(),
            gguf_path: PathBuf::from("models/OldPlanner.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 32.0,
            sha256: "b".repeat(64),
            verified_at: Some(Utc::now()),
            verified_sha256: None,
            source_url: "https://example.com/OldPlanner.gguf".to_string(),
            assigned_backends: vec!["planner".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        let planner = cfg.backends.iter_mut().find(|b| b.id == "planner").unwrap();
        planner.model_file = PathBuf::from("models/OldPlanner.gguf");
        planner.flags.tensor_split = vec![0.5, 0.5];
        planner.flags.split_mode = SplitMode::Layer;
        planner.raw_flags = None;
        planner.flags_advanced = true;
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "qwen3-27b-planner".to_string(),
                gguf_file_name: "NewPlanner.gguf".to_string(),
                quant: "IQ3_S".to_string(),
                params_b: 27.0,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewPlanner.gguf".to_string(),
                assigned_backends: vec!["planner".to_string()],
                enabled: true,
            },
            backend_id: "planner".to_string(),
            clear_split: true,
            raw_flags: Some("-ot exps=CPU".to_string()),
        };
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let planner = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
        // Identity realigned; flags untouched.
        assert_eq!(planner.model_file, PathBuf::from("models/NewPlanner.gguf"));
        assert_eq!(planner.flags.tensor_split, vec![0.5, 0.5]);
        assert_eq!(planner.flags.split_mode, SplitMode::Layer);
        assert_eq!(planner.raw_flags, None);
    }

    #[test]
    fn reset_stale_tool_call_probes_only_on_model_or_raw_change() {
        let probe = Some(ToolCallProbe {
            outcome: ToolCallProbeOutcome::Unparsed,
            at_ms: 42,
        });
        let mut previous = AppConfig::default_config();
        for b in previous.backends.iter_mut() {
            b.tool_call_probe = probe;
        }
        // Identical config: nothing resets.
        let mut current = previous.clone();
        assert!(!reset_stale_tool_call_probes(&previous, &mut current));
        assert!(current.backends.iter().all(|b| b.tool_call_probe == probe));
        // One backend's raw flags change, another's model file:
        // exactly those two probes reset.
        let mut current = previous.clone();
        current.backends[0].raw_flags = Some("--threads 8".to_string());
        current.backends[1].model_file = PathBuf::from("models/other.gguf");
        assert!(reset_stale_tool_call_probes(&previous, &mut current));
        assert_eq!(current.backends[0].tool_call_probe, None);
        assert_eq!(current.backends[1].tool_call_probe, None);
        assert_eq!(current.backends[2].tool_call_probe, probe);
        assert_eq!(current.backends[3].tool_call_probe, probe);
    }

    #[test]
    fn append_missing_catalog_models_appends_only_missing() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "existing".to_string(),
            gguf_path: PathBuf::from("models/existing.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: String::new(),
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
        let entry = |id: &str, enabled: bool| CatalogModelEntry {
            model_id: id.to_string(),
            gguf_file_name: format!("{id}.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 31.6,
            sha256: "a".repeat(64),
            source_url: "https://example.com/x.gguf".to_string(),
            assigned_backends: Vec::new(),
            enabled,
        };
        // "existing" is skipped; "new-opt-in" is appended disabled.
        let changed = append_missing_catalog_models(
            &mut cfg,
            &[entry("existing", true), entry("new-opt-in", false)],
        );
        assert!(changed);
        assert_eq!(cfg.models.len(), 2);
        let added = cfg.models.iter().find(|m| m.id == "new-opt-in").unwrap();
        assert!(!added.enabled);
        assert_eq!(added.gguf_path, PathBuf::from("models/new-opt-in.gguf"));
        assert_eq!(added.params_b, 31.6);
        // A second run appends nothing.
        assert!(!append_missing_catalog_models(
            &mut cfg,
            &[entry("new-opt-in", false)]
        ));
    }

    #[test]
    fn refresh_catalog_definition_replaces_pin_and_serving() {
        let mut cfg = AppConfig::default_config();
        // Stored state as seeded under the old definitions: the
        // planner model carries the old pin and is verified; the
        // planner backend points at the old file and carries a
        // tensor split from the old model's serving mode.
        cfg.models.push(ModelConfig {
            id: "qwen3-27b-planner".to_string(),
            gguf_path: PathBuf::from("models/OldPlanner.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 32.0,
            sha256: "b".repeat(64),
            verified_at: Some(Utc::now()),
            verified_sha256: None,
            source_url: "https://example.com/OldPlanner.gguf".to_string(),
            assigned_backends: vec!["planner".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        let planner = cfg.backends.iter_mut().find(|b| b.id == "planner").unwrap();
        planner.model_file = PathBuf::from("models/OldPlanner.gguf");
        planner.flags.tensor_split = vec![0.5, 0.5];
        planner.flags.split_mode = SplitMode::Layer;
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "qwen3-27b-planner".to_string(),
                gguf_file_name: "NewPlanner.gguf".to_string(),
                quant: "IQ3_S".to_string(),
                params_b: 27.0,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewPlanner.gguf".to_string(),
                assigned_backends: vec!["planner".to_string()],
                enabled: true,
            },
            backend_id: "planner".to_string(),
            clear_split: true,
            raw_flags: None,
        };
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let model = cfg
            .models
            .iter()
            .find(|m| m.id == "qwen3-27b-planner")
            .unwrap();
        assert_eq!(model.gguf_path, PathBuf::from("models/NewPlanner.gguf"));
        assert_eq!(model.sha256, "c".repeat(64));
        assert_eq!(model.quant, "IQ3_S");
        assert!(model.verified_at.is_none());
        let planner = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
        assert_eq!(planner.model_file, PathBuf::from("models/NewPlanner.gguf"));
        assert!(planner.flags.tensor_split.is_empty());
        assert_eq!(planner.flags.split_mode, SplitMode::None);
        // Same pin again: no-op.
        assert!(!refresh_catalog_definition(&mut cfg, &refresh));
    }

    #[test]
    fn refresh_catalog_definition_preserves_operator_changes() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-coder-30b".to_string(),
            gguf_path: PathBuf::from("models/OldCoder.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: "b".repeat(64),
            verified_at: Some(Utc::now()),
            verified_sha256: None,
            source_url: "https://example.com/OldCoder.gguf".to_string(),
            assigned_backends: vec!["coder".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        // The operator moved the coder backend to another model's
        // file and set their own raw flags.
        let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        coder.model_file = PathBuf::from("models/SomethingElse.gguf");
        coder.raw_flags = Some("--threads 8".to_string());
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "qwen3-coder-30b".to_string(),
                gguf_file_name: "NewCoder.gguf".to_string(),
                quant: "UD-Q4_K_M".to_string(),
                params_b: 80.0,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewCoder.gguf".to_string(),
                assigned_backends: vec!["coder".to_string()],
                enabled: true,
            },
            backend_id: "coder".to_string(),
            clear_split: true,
            raw_flags: Some("-ot exps=CPU".to_string()),
        };
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let coder = cfg.backends.iter().find(|b| b.id == "coder").unwrap();
        // Not realigned and no serving change (operator's assignment
        // stands): the file and the operator's raw flags are kept.
        assert_eq!(coder.model_file, PathBuf::from("models/SomethingElse.gguf"));
        assert_eq!(coder.raw_flags.as_deref(), Some("--threads 8"));
        // A model the configuration does not have is a no-op.
        let missing = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "not-present".to_string(),
                gguf_file_name: "X.gguf".to_string(),
                quant: "Q4_K_M".to_string(),
                params_b: 1.0,
                sha256: "d".repeat(64),
                source_url: String::new(),
                assigned_backends: Vec::new(),
                enabled: true,
            },
            backend_id: "coder".to_string(),
            clear_split: false,
            raw_flags: None,
        };
        assert!(!refresh_catalog_definition(&mut cfg, &missing));
    }

    #[test]
    fn refresh_applies_catalog_offload_only_without_operator_flags() {
        // The catalog default offload rides a definition refresh
        // as raw_flags (e.g. Nemotron's --n-cpu-moe 16): it lands
        // on a serving backend that has no raw flags of its own,
        // and never replaces an operator-changed offload.
        let build = |backend_raw: Option<&str>| {
            let mut cfg = AppConfig::default_config();
            cfg.models.push(ModelConfig {
                id: "nemotron-3.5-lightning-30b".to_string(),
                gguf_path: PathBuf::from("models/OldNemotron.gguf"),
                quant: "Q4_K_M".to_string(),
                params_b: 30.0,
                sha256: "b".repeat(64),
                verified_at: Some(Utc::now()),
                verified_sha256: None,
                source_url: "https://example.com/OldNemotron.gguf".to_string(),
                assigned_backends: vec!["coder".to_string()],
                enabled: true,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
                mtp: None,
            });
            let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
            coder.model_file = PathBuf::from("models/OldNemotron.gguf");
            coder.raw_flags = backend_raw.map(str::to_string);
            cfg
        };
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "nemotron-3.5-lightning-30b".to_string(),
                gguf_file_name: "NewNemotron.gguf".to_string(),
                quant: "Q4_K_M".to_string(),
                params_b: 31.6,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewNemotron.gguf".to_string(),
                assigned_backends: vec!["coder".to_string()],
                enabled: true,
            },
            backend_id: "coder".to_string(),
            clear_split: false,
            raw_flags: Some("--n-cpu-moe 16".to_string()),
        };
        // No operator flags: the catalog default offload applies.
        let mut cfg = build(None);
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let coder = cfg.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
        // Operator-changed offload: preserved verbatim.
        let mut cfg = build(Some("--n-cpu-moe 4"));
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let coder = cfg.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 4"));
    }

    #[test]
    fn repair_catalog_defaults_heals_stale_seed() {
        // A config as an older version wrote it: placeholder hash,
        // params_b 0, no backend assignment, backend disabled with the
        // untouched default model_file (the state that made every
        // backend enable fail validation).
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/Qwen3-8B-Q4_K_M.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 0.0,
            sha256: "PLACEHOLDER-pin".to_string(),
            verified_at: None,
            verified_sha256: None,
            source_url: "https://example.com/q.gguf".to_string(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        let catalog = vec![CatalogModelDefaults {
            model_id: "qwen3-8b".to_string(),
            sha256: "a".repeat(64),
            params_b: 8.0,
            backend_id: "tool-runner".to_string(),
        }];
        assert!(repair_catalog_defaults(
            &mut cfg,
            &catalog,
            "PLACEHOLDER-pin"
        ));
        let m = &cfg.models[0];
        assert_eq!(m.sha256, "a".repeat(64));
        assert_eq!(m.params_b, 8.0);
        assert_eq!(m.assigned_backends, vec!["tool-runner".to_string()]);
        let b = cfg.backends.iter().find(|b| b.id == "tool-runner").unwrap();
        assert!(b.enabled);
        assert_eq!(b.model_file, PathBuf::from("models/Qwen3-8B-Q4_K_M.gguf"));
        validate(&cfg).expect("repaired config validates");
        // A second pass changes nothing.
        assert!(!repair_catalog_defaults(
            &mut cfg,
            &catalog,
            "PLACEHOLDER-pin"
        ));
    }

    #[test]
    fn repair_preserves_custom_hash_and_model_file() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/custom.gguf"),
            quant: "Q8_0".to_string(),
            params_b: 8.0,
            sha256: String::new(), // deliberately unpinned
            verified_at: None,
            verified_sha256: None,
            source_url: "https://example.com/q.gguf".to_string(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        // The operator pointed the backend at a custom file; neither
        // the file nor the model's hash/params may be rewritten. (The
        // one-time pass does enable the default backend — that is its
        // documented purpose.)
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap()
            .model_file = PathBuf::from("models/custom.gguf");
        let catalog = vec![CatalogModelDefaults {
            model_id: "qwen3-8b".to_string(),
            sha256: "a".repeat(64),
            params_b: 8.0,
            backend_id: "tool-runner".to_string(),
        }];
        repair_catalog_defaults(&mut cfg, &catalog, "PLACEHOLDER-pin");
        assert_eq!(cfg.models[0].sha256, "");
        assert_eq!(cfg.models[0].params_b, 8.0);
        let b = cfg.backends.iter().find(|b| b.id == "tool-runner").unwrap();
        assert_eq!(b.model_file, PathBuf::from("models/custom.gguf"));
    }
}
