//! Installed-model registry and the guarded model-file delete.

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Model registry
// ---------------------------------------------------------------------------

/// A model file present on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledModel {
    pub spec: ModelSpec,
    pub path: PathBuf,
    /// RFC 3339 timestamp of the last successful checksum verification.
    pub verified_at: Option<String>,
    /// Backend ids this file is assigned to.
    pub backends: Vec<String>,
}

/// In-memory registry of installed models and their status.
///
/// Persistence of the registry across restarts is manager-config's job;
/// this tracks live install/verify/assign state.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    models: HashMap<String, InstalledModel>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a downloaded model file.
    pub fn register(&mut self, spec: ModelSpec, path: PathBuf) -> &InstalledModel {
        let id = spec.id.clone();
        self.models.entry(id.clone()).or_insert(InstalledModel {
            spec,
            path,
            verified_at: None,
            backends: Vec::new(),
        })
    }

    /// Mark a model checksum-verified now. Call after [`verify_sha256`].
    pub fn mark_verified(&mut self, id: &str) -> Result<(), ModelError> {
        let model = self
            .models
            .get_mut(id)
            .ok_or_else(|| ModelError::UnknownModel(id.to_string()))?;
        model.verified_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(())
    }

    /// Assign an installed model file to a backend.
    pub fn assign_backend(&mut self, id: &str, backend_id: &str) -> Result<(), ModelError> {
        let model = self
            .models
            .get_mut(id)
            .ok_or_else(|| ModelError::UnknownModel(id.to_string()))?;
        if !model.backends.iter().any(|b| b == backend_id) {
            model.backends.push(backend_id.to_string());
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&InstalledModel> {
        self.models.get(id)
    }

    pub fn list(&self) -> Vec<&InstalledModel> {
        let mut models: Vec<&InstalledModel> = self.models.values().collect();
        models.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
        models
    }

    pub fn remove(&mut self, id: &str) -> Option<InstalledModel> {
        self.models.remove(id)
    }
}

// ---------------------------------------------------------------------------
// Guarded delete (spec §14 `model_delete`)
// ---------------------------------------------------------------------------

/// Report of a successful guarded model deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteReport {
    pub id: String,
    pub path: PathBuf,
    /// False when the file was already gone (the desired end state).
    pub file_existed: bool,
    pub bytes_freed: u64,
}

/// Path comparison key for the reference guard: separators unified and
/// case folded — the same normalization `manager_config::validate`
/// applies when comparing a backend's `model_file` to a model's
/// `gguf_path` (replicated here; the original lives in manager-config).
fn normalize_model_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Guarded model deletion.
///
/// Refuses when any backend references the model — either by pointing its
/// `model_file` at the model's `gguf_path` (compared after path
/// normalization), or by appearing in the model's `assigned_backends`.
/// Otherwise deletes the file at `file_path` (a missing file is not an
/// error: the desired end state is already reached) and returns a
/// [`DeleteReport`].
///
/// Removing the model from the caller's registry / config is the caller's
/// job, after this returns `Ok`.
pub fn delete_model_file(
    model: &manager_config::ModelConfig,
    backends: &[manager_config::BackendConfig],
    file_path: &Path,
) -> Result<DeleteReport, ModelError> {
    let target = normalize_model_path(&model.gguf_path);
    let referencing: Vec<String> = backends
        .iter()
        .filter(|b| {
            normalize_model_path(&b.model_file) == target
                || model.assigned_backends.iter().any(|a| a == &b.id)
        })
        .map(|b| b.id.clone())
        .collect();
    if !referencing.is_empty() {
        return Err(ModelError::InUse {
            id: model.id.clone(),
            backends: referencing,
        });
    }
    let (file_existed, bytes_freed) = match std::fs::metadata(file_path) {
        Ok(meta) => {
            let bytes = meta.len();
            std::fs::remove_file(file_path)?;
            (true, bytes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, 0),
        Err(e) => return Err(ModelError::Io(e.to_string())),
    };
    Ok(DeleteReport {
        id: model.id.clone(),
        path: file_path.to_path_buf(),
        file_existed,
        bytes_freed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;

    #[test]
    fn registry_lifecycle() {
        let mut registry = ModelRegistry::new();
        let spec = test_spec("qwen3-8b", 5_280_000_000);
        registry.register(spec.clone(), PathBuf::from("/models/qwen3-8b.gguf"));
        assert!(registry.get(&spec.id).unwrap().verified_at.is_none());

        registry.mark_verified(&spec.id).unwrap();
        assert!(registry.get(&spec.id).unwrap().verified_at.is_some());

        registry.assign_backend(&spec.id, "planner").unwrap();
        registry.assign_backend(&spec.id, "planner").unwrap(); // idempotent
        assert_eq!(
            registry.get(&spec.id).unwrap().backends,
            vec!["planner".to_string()]
        );

        assert!(matches!(
            registry.mark_verified("nope"),
            Err(ModelError::UnknownModel(_))
        ));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.remove(&spec.id).is_some());
        assert!(registry.get(&spec.id).is_none());
    }

    // -- delete_model_file --------------------------------------------------

    fn test_model_config(id: &str, path: PathBuf) -> manager_config::ModelConfig {
        manager_config::ModelConfig {
            id: id.to_string(),
            gguf_path: path,
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: "00".repeat(32),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        }
    }

    fn test_backend(id: &str, model_file: PathBuf) -> manager_config::BackendConfig {
        manager_config::BackendConfig {
            id: id.to_string(),
            enabled: true,
            model_file,
            port: 8081,
            flags: manager_config::ServerFlags {
                n_ctx: 32768,
                n_batch: 512,
                tensor_split: Vec::new(),
                split_mode: manager_config::SplitMode::None,
            },
            raw_flags: None,
            flags_advanced: false,
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            tool_call_probe: None,
            purpose: String::new(),
        }
    }

    fn temp_gguf(id: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("llm-manager-test-{id}.gguf"));
        std::fs::write(&path, bytes).expect("write temp gguf");
        path
    }

    #[test]
    fn delete_model_file_removes_file_and_reports_bytes() {
        let path = temp_gguf("delete-ok", b"gguf-bytes");
        let model = test_model_config("m1", path.clone());
        let backends = vec![test_backend("planner", PathBuf::from("/other/model.gguf"))];

        let report = delete_model_file(&model, &backends, &path).expect("delete");
        assert_eq!(report.id, "m1");
        assert_eq!(report.path, path);
        assert!(report.file_existed);
        assert_eq!(report.bytes_freed, 10);
        assert!(!path.exists(), "file must be gone");
    }

    #[test]
    fn delete_model_file_missing_file_is_not_an_error() {
        let path = std::env::temp_dir().join("llm-manager-test-delete-missing.gguf");
        let _ = std::fs::remove_file(&path);
        let model = test_model_config("m2", path);

        let report = delete_model_file(&model, &[], &model.gguf_path.clone()).expect("delete");
        assert!(!report.file_existed);
        assert_eq!(report.bytes_freed, 0);
    }

    #[test]
    fn delete_model_file_refuses_when_backend_points_at_file() {
        let path = temp_gguf("delete-refused-path", b"gguf-bytes");
        let model = test_model_config("m3", path.clone());
        let backends = vec![test_backend("planner", path.clone())];

        let err = delete_model_file(&model, &backends, &path).expect_err("must refuse");
        assert!(matches!(err, ModelError::InUse { .. }));
        assert!(err.to_string().contains("planner"));
        assert!(path.exists(), "refused delete must leave the file");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn delete_model_file_refuses_with_normalized_path_match() {
        let path = temp_gguf("delete-refused-normalized", b"gguf-bytes");
        let model = test_model_config("m5", path.clone());
        // Same file, spelled with different case and separators: the
        // guard compares normalized paths, so this still refuses.
        let spelled = path.to_string_lossy().replace('/', "\\").to_uppercase();
        let backends = vec![test_backend("planner", PathBuf::from(spelled))];

        let err = delete_model_file(&model, &backends, &path).expect_err("must refuse");
        assert!(matches!(err, ModelError::InUse { .. }));
        assert!(path.exists(), "refused delete must leave the file");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn delete_model_file_refuses_when_backend_is_assigned() {
        let path = temp_gguf("delete-refused-assign", b"gguf-bytes");
        let mut model = test_model_config("m4", path.clone());
        model.assigned_backends.push("coder".to_string());
        // Backend points at a *different* file: the assignment alone guards.
        let backends = vec![test_backend("coder", PathBuf::from("/other/model.gguf"))];

        let err = delete_model_file(&model, &backends, &path).expect_err("must refuse");
        assert!(matches!(err, ModelError::InUse { .. }));
        assert!(path.exists(), "refused delete must leave the file");
        std::fs::remove_file(&path).expect("cleanup");
    }
}
