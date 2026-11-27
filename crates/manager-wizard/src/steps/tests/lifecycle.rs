use super::*;

/// Install-mode smoke tests: the step waits for every enabled
/// member backend to answer /health before probing. Script all
/// four default backends healthy.
pub(super) fn script_backends_healthy(ops: &FakeSystemOps) {
    for port in [8081u16, 8082, 8083, 8084] {
        ops.with_http_get(
            &format!("http://127.0.0.1:{port}/health"),
            FakeSystemOps::http_ok("{}"),
        );
    }
}

#[tokio::test]
async fn smoke_test_passes_and_writes_baselines() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_post(
        "http://127.0.0.1:4000/v1/chat/completions",
        smoke_post_response(),
    );
    script_backends_healthy(&ops);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = SmokeTest::default();
    assert_eq!(step.planned_commands(&ctx).len(), 4, "one probe per group");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("4 group(s)"));
    assert_eq!(ops.posts().len(), 4);
    // Tag present in every request body.
    for (_, body) in ops.posts() {
        assert!(body.contains("wizard smoke test wizard-"), "{body}");
    }
    let raw = ops
        .file_bytes(&PathBuf::from("/data/wizard/smoke-baselines.json"))
        .expect("baselines written");
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["baselines"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn smoke_test_fails_on_empty_completion() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_post(
        "http://127.0.0.1:4000/v1/chat/completions",
        FakeSystemOps::http_ok(r#"{"choices":[{"message":{"content":"  "}}]}"#),
    );
    script_backends_healthy(&ops);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = SmokeTest::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("empty completion"));
}

#[tokio::test]
async fn smoke_test_passes_on_reasoning_only_completion() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            FakeSystemOps::http_ok(
                r#"{"choices":[{"message":{"content":"","reasoning_content":"thinking about the probe"},"finish_reason":"length"}]}"#,
            ),
        );
    script_backends_healthy(&ops);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = SmokeTest::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail.iter().any(|l| l.contains("reasoning-only")),
        "{out:?}"
    );
    // The probe asks the backend not to think and gives the
    // reply room to arrive.
    let posts = ops.posts();
    assert!(posts[0].1.contains("enable_thinking"), "{}", posts[0].1);
    assert!(posts[0].1.contains("\"max_tokens\":128"), "{}", posts[0].1);
}

#[tokio::test]
async fn smoke_test_fails_on_http_error_status() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_post(
        "http://127.0.0.1:4000/v1/chat/completions",
        crate::HttpResponse {
            status: 500,
            body: b"boom".to_vec(),
            elapsed_ms: 5,
        },
    );
    script_backends_healthy(&ops);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = SmokeTest::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("500"));
}

// --- Step registry invariants ---

#[test]
fn default_steps_have_unique_ids_and_valid_prereq_order() {
    let steps = default_steps();
    assert_eq!(steps.len(), 12);
    let ids: Vec<&str> = steps.iter().map(|s| s.id()).collect();
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        assert!(seen.insert(*id), "duplicate id {id}");
    }
    // Every prerequisite names an earlier step (topological order).
    for (i, s) in steps.iter().enumerate() {
        for pre in s.prerequisites() {
            let pos = ids.iter().position(|id| *id == pre.as_str());
            assert!(
                pos.map(|p| p < i).unwrap_or(false),
                "step {} has bad prerequisite {pre}",
                s.id()
            );
        }
    }
    // All eleven are checks (audit covers everything).
    assert!(steps.iter().all(|s| s.is_check()));
}

#[test]
fn every_step_has_a_remediation_hint() {
    for s in default_steps() {
        assert!(
            s.remediation_hint().is_some(),
            "step {} lacks a remediation hint",
            s.id()
        );
    }
}

#[test]
fn planned_commands_match_mode() {
    let _guard = PathGuard::set_to(&path_dir_with_uv(true));
    // Install mode: StartGateway shows the five commands it may
    // spawn (two venv probes, venv creation, pip install, spawn)...
    let install_ctx = WizardContext::new(
        test_config_with_models(),
        PathBuf::from("/data"),
        RunMode::Install,
        Arc::new(FakeSystemOps::new()),
    );
    let install_cmds = StartGateway::default().planned_commands(&install_ctx);
    assert_eq!(install_cmds.len(), 7);
    assert!(install_cmds[2].argv[0].ends_with(StartGateway::uv_binary_name()));

    // ...audit mode shows only the health probe.
    let audit_ctx = WizardContext::new(
        test_config_with_models(),
        PathBuf::from("/data"),
        RunMode::Audit,
        Arc::new(FakeSystemOps::new()),
    );
    let audit_cmds = StartGateway::default().planned_commands(&audit_ctx);
    assert_eq!(audit_cmds.len(), 1);
    assert_eq!(audit_cmds[0].argv[0], "http-get");

    // VscodeExtensions: install commands vs list-extensions probe.
    let install_cmds = VscodeExtensions::default().planned_commands(&install_ctx);
    assert!(install_cmds.iter().all(|c| c.argv[0] == "code"));
    assert!(install_cmds
        .iter()
        .all(|c| c.argv.contains(&"--install-extension".to_string())));
    let audit_cmds = VscodeExtensions::default().planned_commands(&audit_ctx);
    assert_eq!(audit_cmds.len(), 1);
    assert_eq!(
        audit_cmds[0].argv,
        vec!["code".to_string(), "--list-extensions".to_string()]
    );

    // DetectHardware: the per-GPU query plus the driver-version header
    // probe that RealSystemOps::gpu_inventory actually runs. argv[0]
    // is telemetry's resolved smi path (bare "nvidia-smi" unless a
    // System32 copy exists).
    let smi = manager_telemetry::default_smi_path()
        .to_string_lossy()
        .into_owned();
    let hw_cmds = DetectHardware.planned_commands(&install_ctx);
    assert_eq!(hw_cmds.len(), 2);
    assert_eq!(hw_cmds[0].argv[0], smi);
    assert!(hw_cmds[0].argv.iter().any(|a| a.contains("--query-gpu")));
    assert_eq!(hw_cmds[1].argv, vec![smi]);
}

// --- Lifecycle (WP3): restart semantics, PostgreSQL adopt/port
// change, honest stop, markers, applied stamp ---

/// A gateway step with fast health polling and no DB sustain wait.
fn fast_gateway_step() -> StartGateway {
    StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    }
}

/// Script an existing venv on the pinned Python with litellm
/// importable: no setup commands run (the fast restart path).
fn with_healthy_venv(ops: &FakeSystemOps, step: &StartGateway, ctx: &WizardContext) {
    ops.with_file(step.venv_python(ctx), Vec::new());
    let probe = step.version_probe_command(ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        FakeSystemOps::ok_output(&format!("{}\n", crate::GATEWAY_PYTHON_VERSION)),
    );
}

fn with_healthy_gateway_http(ops: &FakeSystemOps) {
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
}

fn gateway_markers(ops: &FakeSystemOps) -> Vec<String> {
    ops.markers_in(&crate::gateway_lifecycle_log(Path::new("/data")))
}

fn postgres_markers(ops: &FakeSystemOps) -> Vec<String> {
    ops.markers_in(&crate::postgres_lifecycle_log(Path::new("/data")))
}

fn has_marker(markers: &[String], needle: &str) -> bool {
    markers
        .iter()
        .any(|m| manager_supervisor::lifecycle::is_marker_line(m) && m.contains(needle))
}

fn pg_ctl_argv(data_dir: &Path, args: &[&str]) -> Vec<String> {
    pg_ctl_command(data_dir, args).argv
}

/// GW-1 / ACT-2 / OS-4: a gateway restart with a healthy venv and
/// a PostgreSQL that already accepts connections adopts the
/// server — no `pg_ctl stop`, no `pg_ctl start` — and still stops
/// the previous gateway exactly once.
#[tokio::test]
async fn start_gateway_restart_adopts_ready_postgres_without_restarting_it() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_healthy_venv(&ops, &step, &ctx);
    with_postgres_installed(&ops, &ctx);
    // The previous gateway is running.
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    // pg_isready answers (unscripted commands succeed); a live
    // postmaster on the configured port.
    ops.with_file(
        postmaster_pid_path(&ctx.data_dir),
        b"5151\n/data/postgres/data\n1700000000\n54329\n".to_vec(),
    );
    ops.with_process_image(5151, "postgres.exe");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_commands();
    let pg_ctl = |verb: &str| {
        executed
            .iter()
            .any(|a| a.first().is_some_and(|p| p.contains("pg_ctl")) && a.iter().any(|x| x == verb))
    };
    assert!(
        !pg_ctl("stop"),
        "PostgreSQL must not be stopped: {executed:?}"
    );
    assert!(
        !pg_ctl("start"),
        "PostgreSQL must not be started: {executed:?}"
    );
    assert!(ops.process_alive(5151), "the postmaster keeps running");
    assert_eq!(ops.killed_pids(), vec![7777]);
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("PostgreSQL already running")),
        "{out:?}"
    );
    assert!(
        has_marker(
            &postgres_markers(&ops),
            "adopted running server: pid 5151, port 54329"
        ),
        "{:?}",
        ops.markers()
    );
    assert!(
        has_marker(&gateway_markers(&ops), "PostgreSQL ready: adopted pid 5151"),
        "{:?}",
        ops.markers()
    );
}

/// GW-2(d): PostgreSQL is ensured BEFORE the previous gateway is
/// stopped, so a database that cannot start leaves the old
/// gateway serving (and the failure lands in the gateway's own
/// lifecycle log and in PostgreSQL's).
#[tokio::test]
async fn start_gateway_postgres_failure_leaves_previous_gateway_serving() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_healthy_venv(&ops, &step, &ctx);
    with_postgres_installed(&ops, &ctx);
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    let isready = pg_isready_argv(&ctx.data_dir);
    ops.with_command(&argv_refs(&isready), pg_not_ready_output());
    let start = postgres_start_command(&ctx.data_dir, 54329).argv;
    ops.with_command_failure(&argv_refs(&start), 1, "could not start server");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(ops.killed_pids().is_empty(), "{:?}", ops.killed_pids());
    assert!(ops.process_alive(7777), "the old gateway keeps serving");
    assert!(ops.spawns().is_empty());
    assert!(
        has_marker(
            &gateway_markers(&ops),
            "gateway: start failed: PostgreSQL failed to start"
        ),
        "{:?}",
        ops.markers()
    );
    assert!(
        has_marker(&postgres_markers(&ops), "postgres: start failed"),
        "{:?}",
        ops.markers()
    );
    assert!(
        ops.file_bytes(&crate::gateway_applied_stamp_path(Path::new("/data")))
            .is_none(),
        "no applied stamp on failure"
    );
}

/// OS-5 / GW-1: a live postmaster listening on a port other than
/// the configured one is stopped (`pg_ctl stop -m fast`) and the
/// server is started on the configured port.
#[tokio::test]
async fn postgres_port_change_restarts_the_server_on_the_new_port() {
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let ctx = pg_ctx(&ops, test_config_with_models(), &data_dir);
    with_postgres_installed(&ops, &ctx);
    ops.with_file(
        postmaster_pid_path(&data_dir),
        b"5151\n/data/postgres/data\n1700000000\n5432\n".to_vec(),
    );
    ops.with_process_image(5151, "postgres.exe");
    let stop = pg_ctl_argv(&data_dir, &["stop", "-m", "fast"]);
    ops.with_command_kills_pid(&argv_refs(&stop), 5151);
    let isready = pg_isready_argv(&data_dir);
    ops.with_command_sequence(
        &argv_refs(&isready),
        vec![pg_not_ready_output(), pg_ready_output()],
    );
    let mut log = Vec::new();
    let (url, how) = ensure_postgres_running_detailed(&ctx, &mut log)
        .await
        .expect("restarted on the configured port");
    assert!(url.contains("127.0.0.1:54329"), "{url}");
    assert!(matches!(how, PostgresReady::Started { .. }), "{how:?}");
    let executed = ops.executed_commands();
    let stop_at = executed
        .iter()
        .position(|a| *a == stop)
        .expect("pg_ctl stop ran");
    let start = postgres_start_command(&data_dir, 54329).argv;
    let start_at = executed
        .iter()
        .position(|a| *a == start)
        .expect("pg_ctl start ran on the new port");
    assert!(stop_at < start_at, "{executed:?}");
    assert!(
        log.iter()
            .any(|l| l.contains("running on port 5432 but the configured port is 54329")),
        "{log:?}"
    );
    let markers = postgres_markers(&ops);
    assert!(has_marker(&markers, "port changed"), "{markers:?}");
    assert!(
        has_marker(&markers, "stopped (pg_ctl stop -m fast)"),
        "{markers:?}"
    );
    assert!(
        has_marker(&markers, "starting: pg_ctl start on port 54329"),
        "{markers:?}"
    );
    assert!(has_marker(&markers, "ready:"), "{markers:?}");
}

/// A postmaster on the configured port is adopted as before (no
/// port-change restart).
#[tokio::test]
async fn postgres_matching_port_is_not_restarted() {
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let ctx = pg_ctx(&ops, test_config_with_models(), &data_dir);
    with_postgres_installed(&ops, &ctx);
    ops.with_file(
        postmaster_pid_path(&data_dir),
        b"5151\n/data/postgres/data\n1700000000\n54329\n".to_vec(),
    );
    ops.with_process_image(5151, "postgres.exe");
    let mut log = Vec::new();
    let (_, how) = ensure_postgres_running_detailed(&ctx, &mut log)
        .await
        .unwrap();
    assert_eq!(how, PostgresReady::Adopted { pid: Some(5151) });
    assert!(
        !ops.executed_commands()
            .iter()
            .any(|a| a.iter().any(|x| x == "stop" || x == "start")),
        "{:?}",
        ops.executed_commands()
    );
}

/// GW-2(a), critic correction: when the venv must be repaired, the
/// previous gateway is stopped BEFORE the setup commands run (and
/// not a second time before the spawn).
#[tokio::test]
async fn start_gateway_venv_repair_stops_previous_gateway_first() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    // No venv: venv creation + package install run.
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.killed_pids(), vec![7777]);
    let stopped_at = out
        .log_tail
        .iter()
        .position(|l| l == "stopped previous gateway (pid 7777)")
        .expect("previous gateway stopped");
    let first_setup = out
        .log_tail
        .iter()
        .position(|l| l.starts_with("running: ") && l.contains(" venv "))
        .expect("venv command ran");
    assert!(stopped_at < first_setup, "{:?}", out.log_tail);
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("before repairing its venv")),
        "{out:?}"
    );
    assert_eq!(
        gateway_markers(&ops)
            .iter()
            .filter(|m| m.contains("stopping previous gateway"))
            .count(),
        1,
        "{:?}",
        ops.markers()
    );
}

fn service_mode_ctx(ops: &Arc<FakeSystemOps>) -> WizardContext {
    let mut cfg = test_config_with_models();
    cfg.supervision.mode = SupervisionMode::Service;
    WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops.clone())
}

/// SCM-7 / CR-7: service mode with the gateway service NOT
/// registered falls back to the process route — the recorded
/// process-mode gateway is stopped by pid (not orphaned by a
/// pid-file delete) and no service stop is claimed.
#[tokio::test]
async fn start_gateway_service_mode_unregistered_falls_back_to_process() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = service_mode_ctx(&ops);
    with_healthy_venv(&ops, &step, &ctx);
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.killed_pids(), vec![7777]);
    assert!(ops.service_stops().is_empty(), "{:?}", ops.service_stops());
    assert!(ops.service_starts().is_empty());
    assert_eq!(ops.spawns().len(), 1, "spawned as a process");
    assert!(
        out.log_tail.iter().any(|l| l.contains("is not registered")),
        "{out:?}"
    );
    assert!(
        !out.log_tail
            .iter()
            .any(|l| l.contains("stopped previous gateway service")),
        "{out:?}"
    );
    assert!(
        has_marker(&gateway_markers(&ops), "spawned: pid 4242 (process)"),
        "{:?}",
        ops.markers()
    );
}

/// CR-7 / SCM-7: on the service route an already-stopped service
/// is logged as "not running" (never as stopped), and a live
/// process-mode gateway left over from before the mode flip is
/// still stopped — the pid file is not deleted out from under it.
#[tokio::test]
async fn start_gateway_service_route_logs_stop_outcome_and_stops_leftover_process() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = service_mode_ctx(&ops);
    with_healthy_venv(&ops, &step, &ctx);
    let svc = manager_services::GATEWAY_SERVICE_NAME;
    ops.with_registered_service(svc);
    ops.with_service_state(svc, manager_services::ServiceState::Stopped);
    ops.with_service_stop_outcome(svc, manager_services::StopOutcome::AlreadyStopped);
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.service_stops(), vec![svc.to_string()]);
    assert_eq!(ops.service_starts(), vec![svc.to_string()]);
    assert!(ops.spawns().is_empty(), "started as a service");
    assert_eq!(ops.killed_pids(), vec![7777]);
    assert!(
        out.log_tail
            .iter()
            .any(|l| l == "gateway service was not running"),
        "{out:?}"
    );
    assert!(
        !out.log_tail
            .iter()
            .any(|l| l.contains("stopped previous gateway service")),
        "{out:?}"
    );
    assert!(
        has_marker(&gateway_markers(&ops), "spawned: pid 4242 (service)"),
        "{:?}",
        ops.markers()
    );
}

/// A running gateway service is stopped and reported with its
/// measured wait.
#[tokio::test]
async fn start_gateway_service_route_reports_real_service_stop() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = service_mode_ctx(&ops);
    with_healthy_venv(&ops, &step, &ctx);
    let svc = manager_services::GATEWAY_SERVICE_NAME;
    ops.with_registered_service(svc);
    ops.with_service_stop_outcome(
        svc,
        manager_services::StopOutcome::Stopped { waited_ms: 420 },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l == "stopped previous gateway service in 420 ms"),
        "{out:?}"
    );
    let markers = gateway_markers(&ops);
    assert!(
        has_marker(&markers, "stopping previous gateway: service"),
        "{markers:?}"
    );
    assert!(
        has_marker(&markers, "previous gateway stopped: service, in 420 ms"),
        "{markers:?}"
    );
}

/// Service route, final re-check: the SCM no longer reporting the
/// service Running fails the step even though the probe answers
/// (it may be answering from another process on the port).
#[tokio::test]
async fn start_gateway_service_route_final_check_requires_running_service() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = service_mode_ctx(&ops);
    with_healthy_venv(&ops, &step, &ctx);
    let svc = manager_services::GATEWAY_SERVICE_NAME;
    ops.with_registered_service(svc);
    // The service reaches Running, then falls back to Stopped
    // while the probe still answers (from another process).
    ops.with_service_state_after_start(svc, manager_services::ServiceState::Stopped);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(
        out.message.contains("the gateway service is stopped"),
        "{out:?}"
    );
    assert!(ops
        .file_bytes(&crate::gateway_applied_stamp_path(Path::new("/data")))
        .is_none());
}

/// CR-4: a gateway still serving on the port with no pid file
/// recording it is found by its port and stopped before the spawn.
#[tokio::test]
async fn start_gateway_stops_unrecorded_gateway_found_by_port() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_healthy_venv(&ops, &step, &ctx);
    ops.with_port_owner(4000, 8888);
    ops.with_process_image(8888, "python.exe");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.killed_pids(), vec![8888]);
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("no pid file records")),
        "{out:?}"
    );
    assert!(
        has_marker(
            &gateway_markers(&ops),
            "stopping previous gateway: pid 8888 (found by port owner)"
        ),
        "{:?}",
        ops.markers()
    );
}

/// An unrelated program on the gateway port (unknown or other
/// image) is never killed by port.
#[tokio::test]
async fn start_gateway_never_kills_a_non_gateway_port_owner() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_healthy_venv(&ops, &step, &ctx);
    ops.with_port_owner(4000, 8888);
    ops.with_process_image(8888, "nginx.exe");
    let _ = step.run(&ctx).await.unwrap();
    assert!(ops.killed_pids().is_empty(), "{:?}", ops.killed_pids());
}

/// CR-1: the applied stamp is written on success, carrying the
/// YAML write instant and the new pid ...
#[tokio::test]
async fn start_gateway_writes_applied_stamp_on_success() {
    let before = chrono::Utc::now();
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    with_healthy_gateway_http(&ops);
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_healthy_venv(&ops, &step, &ctx);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let stamp = ops
        .file_bytes(&crate::gateway_applied_stamp_path(Path::new("/data")))
        .expect("applied stamp written");
    let text = String::from_utf8(stamp).unwrap();
    let at = crate::parse_gateway_applied_at(&text).expect("stamp parses");
    assert!(at >= before && at <= chrono::Utc::now(), "{text}");
    assert!(text.contains("4242"), "{text}");
    let markers = gateway_markers(&ops);
    assert!(
        has_marker(&markers, "start-gateway step started"),
        "{markers:?}"
    );
    assert!(
        has_marker(&markers, "spawned: pid 4242 (process)"),
        "{markers:?}"
    );
    assert!(
        has_marker(&markers, "healthy: pid 4242 after"),
        "{markers:?}"
    );
    assert!(
        has_marker(&markers, "started: pid 4242 serving on :4000"),
        "{markers:?}"
    );
    assert!(!has_marker(&markers, "start failed"), "{markers:?}");
}

/// ... and NOT when the gateway never becomes healthy (the YAML
/// was already rewritten by then), where a failure marker is
/// written instead.
#[tokio::test]
async fn start_gateway_skips_applied_stamp_when_unhealthy() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    // No healthy answer scripted: the health wait fails.
    let step = fast_gateway_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_healthy_venv(&ops, &step, &ctx);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(ops
        .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
        .is_some());
    assert!(ops
        .file_bytes(&crate::gateway_applied_stamp_path(Path::new("/data")))
        .is_none());
    let markers = gateway_markers(&ops);
    assert!(has_marker(&markers, "spawned: pid 4242"), "{markers:?}");
    assert!(
        has_marker(&markers, "start failed: gateway did not become healthy"),
        "{markers:?}"
    );
}

#[test]
fn postmaster_pid_parses_pid_and_port() {
    assert_eq!(
        parse_postmaster_pid("5151\n/data\n1700000000\n5432\n/tmp\nlocalhost\n"),
        Some(PostmasterInfo {
            pid: 5151,
            port: Some(5432)
        })
    );
    assert_eq!(
        parse_postmaster_pid("5151\n/data\n"),
        Some(PostmasterInfo {
            pid: 5151,
            port: None
        })
    );
    assert_eq!(
        parse_postmaster_pid("5151\r\n/data\r\n1\r\n 6543 \r\n"),
        Some(PostmasterInfo {
            pid: 5151,
            port: Some(6543)
        })
    );
    assert_eq!(parse_postmaster_pid("garbage\n"), None);
    assert_eq!(parse_postmaster_pid(""), None);
}

#[test]
fn postgres_status_port_mismatch_needs_a_live_postmaster() {
    let mut st = PostgresStatus {
        installed: true,
        pid: Some(1),
        pid_alive: true,
        pid_port: Some(5432),
        configured_port: 54329,
        accepting: false,
    };
    assert!(st.port_mismatch());
    st.pid_port = Some(54329);
    assert!(!st.port_mismatch());
    st.pid_port = Some(5432);
    st.pid_alive = false;
    assert!(!st.port_mismatch());
    st.pid_alive = true;
    st.pid_port = None;
    assert!(!st.port_mismatch());
}

#[tokio::test]
async fn postgres_status_reports_not_installed() {
    let ops = FakeSystemOps::new();
    let st = postgres_status(&ops, &test_config_with_models(), Path::new("/data")).await;
    assert!(!st.installed);
    assert_eq!(st.pid, None);
    assert!(!st.accepting);
}

#[tokio::test]
async fn stop_postgres_server_writes_markers() {
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let ctx = pg_ctx(&ops, test_config_with_models(), &data_dir);
    with_postgres_installed(&ops, &ctx);
    assert!(stop_postgres_server(ops.as_ref(), &data_dir).await.unwrap());
    let stop = pg_ctl_argv(&data_dir, &["stop", "-m", "fast"]);
    ops.with_command(
        &argv_refs(&stop),
        CommandOutput {
            exit_code: 3,
            stdout: String::new(),
            stderr: "pg_ctl: no server running".to_string(),
        },
    );
    assert!(!stop_postgres_server(ops.as_ref(), &data_dir).await.unwrap());
    let markers = postgres_markers(&ops);
    assert!(
        has_marker(&markers, "postgres: stopped (pg_ctl stop -m fast)"),
        "{markers:?}"
    );
    assert!(
        has_marker(&markers, "postgres: stop: no server was running"),
        "{markers:?}"
    );
}
