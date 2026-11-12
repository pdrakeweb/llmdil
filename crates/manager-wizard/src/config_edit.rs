//! Typed configuration edits returned by wizard steps (code-review PS-4).
//!
//! Steps run against a snapshot clone of the app config
//! ([`WizardContext::config`](crate::WizardContext)), possibly for
//! minutes. A step that persisted its own load-modify-save of
//! `config.json` raced every config write the app made meanwhile (a
//! lost update) and, before the guard in
//! [`steps`](crate::steps), could save its stale snapshot over a file
//! that failed to load. Instead, a step describes WHAT it changes as
//! [`ConfigEdit`]s on its [`StepOutcome`](crate::StepOutcome), and
//! whoever owns the configuration applies them:
//!
//! - The app shell ([`ConfigEditMode::ReturnToCaller`]) applies them
//!   under its config write lock: lock → clone → apply → save →
//!   install, so concurrent app-side changes survive.
//! - Without an app shell ([`ConfigEditMode::SaveToDisk`], the
//!   default for [`WizardContext::new`](crate::WizardContext::new)),
//!   the step itself loads `<data_dir>/config.json`, applies the
//!   edits, and saves — never over a file that failed to load.
//!
//! Edits target an entity by id and set only the fields the step
//! owns. An edit whose target no longer exists (deleted while the
//! step ran) is skipped with a log line, never an error.

use chrono::{DateTime, Utc};
use manager_config::{AppConfig, SplitMode};
use serde::{Deserialize, Serialize};

/// Who persists a step's [`ConfigEdit`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigEditMode {
    /// The step loads `<data_dir>/config.json`, applies its edits and
    /// saves (no app shell holds the configuration: unit tests,
    /// one-shot runs). The default.
    #[default]
    SaveToDisk,
    /// The step leaves the disk alone and returns its edits on the
    /// outcome; the caller applies them under its own config lock.
    ReturnToCaller,
}

/// One configuration change a wizard step made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigEdit {
    /// Placement computed by the tensor-split step for one backend.
    BackendPlacement {
        backend_id: String,
        tensor_split: Vec<f32>,
        split_mode: SplitMode,
    },
    /// A model's file was verified against its pinned SHA-256.
    ModelVerified {
        model_id: String,
        verified_at: DateTime<Utc>,
        /// The pinned hash the file was verified against.
        verified_sha256: String,
    },
}

impl ConfigEdit {
    /// Human-readable target, for log lines.
    pub fn target(&self) -> String {
        match self {
            ConfigEdit::BackendPlacement { backend_id, .. } => format!("backend {backend_id}"),
            ConfigEdit::ModelVerified { model_id, .. } => format!("model {model_id}"),
        }
    }
}

/// Result of [`apply_config_edits`].
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AppliedEdits {
    /// Edits whose target existed and was updated.
    pub applied: usize,
    /// One line per edit: what was applied or why it was skipped.
    pub log: Vec<String>,
}

/// Apply `edits` to `cfg` in order. A target id that no longer
/// exists is skipped and logged; nothing here fails.
pub fn apply_config_edits(cfg: &mut AppConfig, edits: &[ConfigEdit]) -> AppliedEdits {
    let mut out = AppliedEdits::default();
    for edit in edits {
        let applied = match edit {
            ConfigEdit::BackendPlacement {
                backend_id,
                tensor_split,
                split_mode,
            } => match cfg.backends.iter_mut().find(|b| &b.id == backend_id) {
                Some(b) => {
                    b.flags.tensor_split = tensor_split.clone();
                    b.flags.split_mode = *split_mode;
                    true
                }
                None => false,
            },
            ConfigEdit::ModelVerified {
                model_id,
                verified_at,
                verified_sha256,
            } => match cfg.models.iter_mut().find(|m| &m.id == model_id) {
                Some(m) => {
                    m.verified_at = Some(*verified_at);
                    // The stamp records WHICH pinned hash the file was
                    // verified against; the audit's verified-by-stamp
                    // shortcut trusts a stamp only while that still
                    // equals the current pin.
                    m.verified_sha256 = Some(verified_sha256.clone());
                    true
                }
                None => false,
            },
        };
        if applied {
            out.applied += 1;
            out.log.push(match edit {
                ConfigEdit::BackendPlacement { backend_id, .. } => {
                    format!("{backend_id}: placement applied to the configuration")
                }
                ConfigEdit::ModelVerified { model_id, .. } => {
                    format!("{model_id}: verified_at stamped")
                }
            });
        } else {
            out.log.push(format!(
                "config edit skipped: {} no longer exists in the configuration",
                edit.target()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_config::AppConfig;

    #[test]
    fn apply_sets_owned_fields_and_skips_missing_targets() {
        let mut cfg = AppConfig::default_config();
        let backend_id = cfg.backends[0].id.clone();
        let now = Utc::now();
        let edits = vec![
            ConfigEdit::BackendPlacement {
                backend_id: backend_id.clone(),
                tensor_split: vec![0.5, 0.5],
                split_mode: SplitMode::Layer,
            },
            ConfigEdit::BackendPlacement {
                backend_id: "deleted-backend".to_string(),
                tensor_split: vec![1.0],
                split_mode: SplitMode::Layer,
            },
            ConfigEdit::ModelVerified {
                model_id: "deleted-model".to_string(),
                verified_at: now,
                verified_sha256: "ab".repeat(32),
            },
        ];
        let before = cfg.clone();
        let out = apply_config_edits(&mut cfg, &edits);
        assert_eq!(out.applied, 1);
        assert_eq!(out.log.len(), 3);
        assert!(out.log[1].contains("deleted-backend"), "{:?}", out.log);
        assert!(out.log[2].contains("deleted-model"), "{:?}", out.log);
        let b = cfg.backends.iter().find(|b| b.id == backend_id).unwrap();
        assert_eq!(b.flags.tensor_split, vec![0.5, 0.5]);
        assert_eq!(b.flags.split_mode, SplitMode::Layer);
        // Nothing else changed.
        assert_eq!(cfg.backends.len(), before.backends.len());
        assert_eq!(
            serde_json::to_value(&cfg.models).unwrap(),
            serde_json::to_value(&before.models).unwrap()
        );
    }

    /// A step that returns one placement edit for the first backend.
    struct EditingStep;

    #[async_trait::async_trait]
    impl crate::WizardStep for EditingStep {
        fn id(&self) -> &str {
            "editing"
        }
        fn name(&self) -> &str {
            "editing"
        }
        fn prerequisites(&self) -> Vec<String> {
            vec![]
        }
        fn planned_commands(&self, _ctx: &crate::WizardContext) -> Vec<crate::PlannedCommand> {
            vec![]
        }
        async fn run(
            &self,
            ctx: &crate::WizardContext,
        ) -> Result<crate::StepOutcome, crate::WizardError> {
            let mut o = crate::StepOutcome::done("edited", vec![]);
            o.config_edits.push(ConfigEdit::BackendPlacement {
                backend_id: ctx.config.backends[0].id.clone(),
                tensor_split: vec![0.25, 0.75],
                split_mode: SplitMode::Layer,
            });
            Ok(o)
        }
    }

    fn editing_ctx(mode: ConfigEditMode) -> crate::WizardContext {
        crate::WizardContext::new(
            AppConfig::default_config(),
            std::path::PathBuf::from("test-data"),
            crate::RunMode::Install,
            std::sync::Arc::new(crate::fake::FakeSystemOps::new()),
        )
        .with_config_edit_mode(mode)
    }

    #[tokio::test]
    async fn wizard_queues_returned_edits_for_the_caller() {
        let mut w = crate::Wizard::new(vec![Box::new(EditingStep)], crate::RunMode::Install)
            .expect("one step");
        let ctx = editing_ctx(ConfigEditMode::ReturnToCaller);
        let out = w.run_step(&ctx, "editing").await.unwrap();
        assert_eq!(out.config_edits.len(), 1);
        let queued = w.take_config_edits();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "editing");
        assert_eq!(queued[0].1, out.config_edits);
        assert!(w.take_config_edits().is_empty(), "drained");

        // A failed application downgrades the step.
        w.fail_step(
            "editing",
            "config not saved".to_string(),
            "save failed".to_string(),
        );
        assert_eq!(w.state("editing"), Some(crate::StepState::Failed));
        let report = w.report();
        let s = report.steps.iter().find(|s| s.id == "editing").unwrap();
        assert_eq!(s.state, crate::StepState::Failed);
        assert_eq!(s.message, "config not saved");
        assert_eq!(s.log_tail.last().map(String::as_str), Some("save failed"));
    }

    #[tokio::test]
    async fn wizard_does_not_queue_edits_a_step_saved_itself() {
        let mut w = crate::Wizard::new(vec![Box::new(EditingStep)], crate::RunMode::Install)
            .expect("one step");
        let ctx = editing_ctx(ConfigEditMode::SaveToDisk);
        let _ = w.run_all(&ctx).await;
        assert!(w.take_config_edits().is_empty());
    }

    #[test]
    fn config_edit_round_trips_as_tagged_json() {
        let edit = ConfigEdit::ModelVerified {
            model_id: "m".to_string(),
            verified_at: Utc::now(),
            verified_sha256: "cd".repeat(32),
        };
        let v = serde_json::to_value(&edit).unwrap();
        assert_eq!(v["kind"], "model_verified");
        let back: ConfigEdit = serde_json::from_value(v).unwrap();
        assert_eq!(back, edit);
    }
}
