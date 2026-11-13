use super::*;
use crate::test_support::*;

/// A wizard step that, while it runs, lets the app make its own
/// config change (clone-save-install, like every op), then returns
/// typed config edits computed from its stale snapshot.
struct ConcurrentEditStep {
    state: Arc<AppState>,
    edits: Vec<manager_wizard::ConfigEdit>,
}

#[async_trait::async_trait]
impl manager_wizard::WizardStep for ConcurrentEditStep {
    fn id(&self) -> &str {
        "concurrent-edit"
    }
    fn name(&self) -> &str {
        "concurrent edit"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![]
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<manager_wizard::PlannedCommand> {
        vec![]
    }
    async fn run(&self, _ctx: &WizardContext) -> Result<StepOutcome, manager_wizard::WizardError> {
        {
            let mut live = self.state.config.write().await;
            let mut new_cfg = live.clone();
            new_cfg.optimize_purpose = "changed while the step ran".to_string();
            manager_config::save(&new_cfg, &self.state.config_path).unwrap();
            *live = new_cfg;
        }
        let mut outcome = StepOutcome::done("edited", vec![]);
        outcome.config_edits = self.edits.clone();
        Ok(outcome)
    }
}

/// Run `ConcurrentEditStep` the way op_run_wizard_step does: a
/// snapshot context in ReturnToCaller mode, then the app applies
/// the returned edits under its config lock.
async fn run_concurrent_edit_step(
    state: &Arc<AppState>,
    edits: Vec<manager_wizard::ConfigEdit>,
) -> (Wizard, Vec<(String, String)>) {
    let ctx = WizardContext::new(
        state.config.read().await.clone(),
        state.data_dir.clone(),
        RunMode::Install,
        Arc::new(manager_wizard::fake::FakeSystemOps::new()),
    )
    .with_config_edit_mode(manager_wizard::ConfigEditMode::ReturnToCaller);
    let mut wizard = Wizard::new(
        vec![Box::new(ConcurrentEditStep {
            state: state.clone(),
            edits,
        })],
        RunMode::Install,
    )
    .unwrap();
    let out = wizard.run_step(&ctx, "concurrent-edit").await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let failed = state.apply_wizard_config_edits(&mut wizard).await;
    (wizard, failed)
}

/// PS-4 (docs/code-review-v0.1.60.md): an app-side config change
/// made while a wizard step runs survives the step's edits — the
/// edits are applied to the live config under the lock, not saved
/// over it from the step's snapshot.
#[tokio::test]
async fn wizard_step_edits_do_not_lose_concurrent_app_changes() {
    let (state, dir) = advanced_test_state(test_cfg());
    ensure_catalog_models(&state).await;
    let backend_id = state.config.read().await.backends[0].id.clone();
    let model_id = state.config.read().await.models[0].id.clone();
    let edits = vec![
        manager_wizard::ConfigEdit::BackendPlacement {
            backend_id: backend_id.clone(),
            tensor_split: vec![0.5, 0.5],
            split_mode: manager_config::SplitMode::Layer,
        },
        manager_wizard::ConfigEdit::ModelVerified {
            model_id: model_id.clone(),
            verified_at: chrono::Utc::now(),
            verified_sha256: "ab".repeat(32),
        },
    ];
    let (wizard, failed) = run_concurrent_edit_step(&state, edits).await;
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(wizard.state("concurrent-edit"), Some(StepState::Done));
    let saved = manager_config::load(&state.config_path).unwrap();
    for cfg in [state.config.read().await.clone(), saved] {
        assert_eq!(cfg.optimize_purpose, "changed while the step ran");
        let b = cfg.backends.iter().find(|b| b.id == backend_id).unwrap();
        assert_eq!(b.flags.tensor_split, vec![0.5, 0.5]);
        let m = cfg.models.iter().find(|m| m.id == model_id).unwrap();
        assert!(m.verified_at.is_some());
        assert_eq!(m.verified_sha256.as_deref(), Some("ab".repeat(32).as_str()));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-4: an edit whose target was deleted while the step ran is
/// skipped, not an error; the step stays Done.
#[tokio::test]
async fn wizard_step_edit_for_a_deleted_target_is_skipped() {
    let (state, dir) = advanced_test_state(test_cfg());
    ensure_catalog_models(&state).await;
    let before = serde_json::to_value(&*state.config.read().await).unwrap();
    let edits = vec![manager_wizard::ConfigEdit::BackendPlacement {
        backend_id: "deleted-while-running".to_string(),
        tensor_split: vec![1.0],
        split_mode: manager_config::SplitMode::Layer,
    }];
    let (wizard, failed) = run_concurrent_edit_step(&state, edits).await;
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(wizard.state("concurrent-edit"), Some(StepState::Done));
    let mut after = serde_json::to_value(&*state.config.read().await).unwrap();
    // Only the concurrent app-side change differs.
    after["optimize_purpose"] = before["optimize_purpose"].clone();
    assert_eq!(after, before);
    let log = std::fs::read_to_string(dir.join("logs").join("wizard.log")).unwrap_or_default();
    assert!(log.contains("deleted-while-running"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-4: when the app cannot save the step's edits, the step is
/// reported failed and the live config does not take the edits.
#[tokio::test]
async fn wizard_step_edits_that_cannot_be_saved_fail_the_step() {
    let (state, dir) = advanced_test_state(test_cfg());
    ensure_catalog_models(&state).await;
    let backend_id = state.config.read().await.backends[0].id.clone();
    let edits = vec![manager_wizard::ConfigEdit::BackendPlacement {
        backend_id: backend_id.clone(),
        tensor_split: vec![0.5, 0.5],
        split_mode: manager_config::SplitMode::Layer,
    }];
    let ctx = WizardContext::new(
        state.config.read().await.clone(),
        state.data_dir.clone(),
        RunMode::Install,
        Arc::new(manager_wizard::fake::FakeSystemOps::new()),
    )
    .with_config_edit_mode(manager_wizard::ConfigEditMode::ReturnToCaller);
    struct EditOnly(Vec<manager_wizard::ConfigEdit>);
    #[async_trait::async_trait]
    impl manager_wizard::WizardStep for EditOnly {
        fn id(&self) -> &str {
            "edit-only"
        }
        fn name(&self) -> &str {
            "edit only"
        }
        fn prerequisites(&self) -> Vec<String> {
            vec![]
        }
        fn planned_commands(&self, _ctx: &WizardContext) -> Vec<manager_wizard::PlannedCommand> {
            vec![]
        }
        async fn run(
            &self,
            _ctx: &WizardContext,
        ) -> Result<StepOutcome, manager_wizard::WizardError> {
            let mut outcome = StepOutcome::done("edited", vec![]);
            outcome.config_edits = self.0.clone();
            Ok(outcome)
        }
    }
    let mut wizard = Wizard::new(vec![Box::new(EditOnly(edits))], RunMode::Install).unwrap();
    let out = wizard.run_step(&ctx, "edit-only").await.unwrap();
    assert_eq!(out.state, StepState::Done);
    // Block the app-side save: the temp file path is a directory
    // (the PS-7 failure injection).
    let mut tmp = state.config_path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::create_dir_all(PathBuf::from(tmp)).unwrap();
    let failed = state.apply_wizard_config_edits(&mut wizard).await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "edit-only");
    assert!(failed[0].1.contains("could not be saved"), "{failed:?}");
    assert_eq!(wizard.state("edit-only"), Some(StepState::Failed));
    let live = state.config.read().await;
    let b = live.backends.iter().find(|b| b.id == backend_id).unwrap();
    assert_ne!(b.flags.tensor_split, vec![0.5, 0.5]);
    drop(live);
    let _ = std::fs::remove_dir_all(&dir);
}
