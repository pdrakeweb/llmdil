use super::*;
use crate::test_support::*;

async fn refusal(state: &Arc<AppState>, op: Op, args: Value) -> Refusal {
    precheck_operation(state, op, &args)
        .await
        .expect_err("refused")
}

#[tokio::test]
async fn busy_services_are_refused_with_their_transition() {
    let (state, dir) = advanced_test_state(test_cfg());
    let planner = json!({ "id": "planner" });
    precheck_operation(&state, Op::StartBackend, &planner)
        .await
        .expect("free");
    let guard = state
        .lifecycle
        .try_begin(
            &lifecycle::backend_key("planner"),
            lifecycle::TransitionKind::Restarting,
            lifecycle::Origin::Api,
        )
        .unwrap();
    for op in [Op::StartBackend, Op::StopBackend, Op::RestartBackend] {
        let r = refusal(&state, op, planner.clone()).await;
        assert!(matches!(r.error, ApiError::Busy(_)), "{:?}", r.error);
        assert!(r.error.to_string().contains("already in progress"));
        assert_eq!(r.details.unwrap()["transition"]["kind"], "restarting");
    }
    // Another backend is not held up.
    precheck_operation(&state, Op::StartBackend, &json!({ "id": "coder" }))
        .await
        .expect("free");
    drop(guard);
    precheck_operation(&state, Op::StartBackend, &planner)
        .await
        .expect("free again");

    let _gateway = state
        .lifecycle
        .try_begin(
            lifecycle::GATEWAY,
            lifecycle::TransitionKind::Starting,
            lifecycle::Origin::Ui,
        )
        .unwrap();
    for (op, args) in [
        (Op::GatewayRestart, json!({})),
        (Op::GatewayStop, json!({})),
        (Op::RunWizardStep, json!({ "step_id": "start-gateway" })),
    ] {
        let r = refusal(&state, op, args).await;
        assert!(matches!(r.error, ApiError::Busy(_)), "{op:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_running_audit_step_or_apply_is_busy() {
    let (state, dir) = advanced_test_state(test_cfg());
    precheck_operation(&state, Op::RunAudit, &json!({}))
        .await
        .expect("free");
    let audit = state.wizard(RunMode::Audit).await.lock().await;
    let r = refusal(&state, Op::RunAudit, json!({})).await;
    assert!(matches!(r.error, ApiError::Busy(_)));
    assert_eq!(r.details.unwrap()["already_running"], true);
    // An audit does not hold up setup steps.
    precheck_operation(
        &state,
        Op::RunWizardStep,
        &json!({ "step_id": "detect-hardware" }),
    )
    .await
    .expect("free");
    drop(audit);

    *state.wizard.step_running.lock().unwrap() =
        Some(("install-mxc".to_string(), std::time::Instant::now()));
    for op in [Op::RunWizardStep, Op::SetupRetryStep] {
        let r = refusal(&state, op, json!({ "step_id": "detect-hardware" })).await;
        assert!(matches!(r.error, ApiError::Busy(_)));
        let details = r.details.unwrap();
        assert_eq!(details["running_step"], "install-mxc");
        assert!(details["elapsed_secs"].is_number());
    }

    let _apply = state.activation.apply_lock.try_lock().unwrap();
    let r = refusal(&state, Op::ActivationApply, json!({})).await;
    assert!(matches!(r.error, ApiError::Busy(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_model_without_a_pin_or_being_verified_is_refused() {
    let mut cfg = test_cfg();
    register_test_model(&mut cfg, "qa-pin", "models/qa-pin.gguf");
    register_test_model(&mut cfg, "qa-nopin", "models/qa-nopin.gguf");
    cfg.models
        .iter_mut()
        .find(|m| m.id == "qa-nopin")
        .unwrap()
        .sha256 = String::new();
    let (state, dir) = advanced_test_state(cfg);
    let verify = |id: &str| json!({ "id": id });
    let r = refusal(&state, Op::ModelVerifyChecksum, verify("qa-nopin")).await;
    assert!(matches!(r.error, ApiError::InvalidInput(_)));
    precheck_operation(&state, Op::ModelVerifyChecksum, &verify("qa-pin"))
        .await
        .expect("pinned");
    state
        .downloads
        .verifying
        .lock()
        .unwrap()
        .insert("qa-pin".to_string());
    let r = refusal(&state, Op::ModelVerifyChecksum, verify("qa-pin")).await;
    assert!(matches!(r.error, ApiError::Busy(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The tasks an operation records are linked while it runs, and a task
/// recorded after the operation's abort starts cancelled.
#[tokio::test]
async fn task_links_collect_recorded_tasks() {
    let (state, dir) = advanced_test_state(test_cfg());
    let link = Arc::new(TaskLink::default());
    let st = Arc::clone(&state);
    let first = with_task_link(Arc::clone(&link), async {
        st.record_task("a", json!({})).await
    })
    .await;
    let outside = state.record_task("b", json!({})).await;
    assert_eq!(link.task_ids(), std::slice::from_ref(&first));
    link.request_abort();
    let late = with_task_link(Arc::clone(&link), async {
        st.record_task("c", json!({})).await
    })
    .await;
    assert!(state.tasks.controls.token(&late).is_cancelled());
    assert!(!state.tasks.controls.token(&first).is_cancelled());
    assert!(!state.tasks.controls.token(&outside).is_cancelled());

    let waiter = tokio::spawn({
        let (state, first) = (Arc::clone(&state), first.clone());
        async move { wait_task_settled(&state, &first).await }
    });
    state
        .finish_task(&first, TaskStatus::Failed, json!({ "error": "no" }))
        .await;
    let settled = waiter.await.unwrap().unwrap();
    assert_eq!(settled.status, TaskStatus::Failed);
    assert_eq!(settled.error.as_deref(), Some("no"));
    assert!(settled.finished_ms.is_some_and(|f| f >= settled.started_ms));
    assert!(wait_task_settled(&state, "task-nope").await.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
