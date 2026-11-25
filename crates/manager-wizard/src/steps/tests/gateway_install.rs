use super::*;

/// F1 (docs/code-review-v0.1.60.md): the gateway venv's LiteLLM
/// requirement is always version-pinned — the default config's
/// pin, an operator pin, or (for a context whose config lost the
/// component or its pin) the built-in default — never bare
/// `litellm[proxy]`.
#[test]
fn litellm_spec_is_always_pinned() {
    let step = StartGateway::default();
    let ops = Arc::new(FakeSystemOps::new());
    let default_pin = format!("litellm[proxy]=={}", manager_config::DEFAULT_LITELLM_PIN);

    let ctx = ctx_with(ops.clone(), RunMode::Install);
    assert_eq!(step.litellm_spec(&ctx), default_pin);

    let mut cfg = test_config_with_models();
    cfg.updates.components.clear();
    let ctx = WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops.clone());
    assert_eq!(step.litellm_spec(&ctx), default_pin);

    let mut cfg = test_config_with_models();
    cfg.updates
        .components
        .get_mut("litellm")
        .expect("default config carries the litellm component")
        .pinned_version = Some("   ".to_string());
    let ctx = WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops.clone());
    assert_eq!(step.litellm_spec(&ctx), default_pin);

    let mut cfg = test_config_with_models();
    cfg.updates
        .components
        .get_mut("litellm")
        .unwrap()
        .pinned_version = Some("1.104.2".to_string());
    let ctx = WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops);
    assert_eq!(step.litellm_spec(&ctx), "litellm[proxy]==1.104.2");
}

#[tokio::test]
async fn start_gateway_install_spawns_and_healthchecks() {
    let _guard = PathGuard::set_to(&path_dir_with_uv(true));
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let planned = step.planned_commands(&ctx);
    assert_eq!(planned.len(), 7);
    // Probes first, then the pinned venv creation, pip, the spawn,
    // and the firewall reconciliation (show rule, elevated script).
    assert!(planned[2].argv[0].ends_with(StartGateway::uv_binary_name()));
    assert!(planned[2].argv.contains(&"--python".to_string()));
    assert!(planned[2]
        .argv
        .contains(&crate::GATEWAY_PYTHON_VERSION.to_string()));
    assert!(planned[2].argv.contains(&"--clear".to_string()));

    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // No venv exists yet, so the probes execute nothing: venv + pip
    // install ran; gateway spawned with the right port.
    let executed = ops.executed_commands();
    assert_eq!(executed[0], planned[2].argv);
    assert_eq!(executed[1], planned[3].argv);
    let spawns = ops.spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0], planned[4].argv);
    assert!(spawns[0].contains(&"4000".to_string()));
    // config.yaml generated from app state.
    let cfg = ops
        .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
        .expect("config written");
    let text = String::from_utf8(cfg).unwrap();
    assert!(text.contains("8081"), "{text}");
}

#[tokio::test]
async fn start_gateway_recreates_venv_on_wrong_python_version() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    // An existing venv whose interpreter reports Python 3.15 — the
    // unpinned failure mode (hiredis has no cp315 wheel). The step
    // must recreate it pinned instead of failing at `uv venv`.
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        crate::CommandOutput {
            exit_code: 0,
            stdout: "3.15\n".to_string(),
            stderr: String::new(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("recreating it with Python")),
        "{out:?}"
    );
    let executed = ops.executed_commands();
    let venv_cmd = executed
        .iter()
        .find(|a| a.iter().any(|x| x == "venv"))
        .expect("venv recreated");
    assert!(venv_cmd.contains(&"--clear".to_string()));
    assert!(venv_cmd.contains(&crate::GATEWAY_PYTHON_VERSION.to_string()));
    assert!(
        executed.iter().any(|a| a.iter().any(|x| x == "install")),
        "pip install ran: {executed:?}"
    );
    assert_eq!(ops.spawns().len(), 1);
}

#[tokio::test]
async fn start_gateway_reuses_current_venv_with_litellm() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    // Venv already on the pinned Python; the version probe reports
    // it and the import probe succeeds by default — no venv or pip
    // command may run, but the gateway still spawns.
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        crate::CommandOutput {
            exit_code: 0,
            stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
            stderr: String::new(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_commands();
    assert!(
        executed
            .iter()
            .all(|a| !a.iter().any(|x| x == "venv" || x == "install")),
        "no venv/pip commands may run: {executed:?}"
    );
    assert_eq!(ops.spawns().len(), 1);
}

#[tokio::test]
async fn start_gateway_reinstalls_when_venv_litellm_differs_from_pin() {
    // Component updates move LiteLLM by moving the pin: a healthy
    // venv on another version must get a package-only install of
    // the pinned version (the venv itself kept), not the skip.
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = fast_step();
    let mut cfg = test_config_with_models();
    cfg.updates.components.insert(
        manager_config::LITELLM_COMPONENT.to_string(),
        manager_config::ComponentUpdate {
            pinned_version: Some("1.106.0".to_string()),
            auto_update: false,
            last_checked: None,
        },
    );
    let ctx = WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops.clone());
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        FakeSystemOps::ok_output(&format!("{}\n", crate::GATEWAY_PYTHON_VERSION)),
    );
    let argv = litellm_version_argv(&step, &ctx);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    ops.with_command_sequence(
        &refs,
        vec![
            FakeSystemOps::ok_output("1.105.1\n"),
            FakeSystemOps::ok_output("1.106.0\n"),
        ],
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_commands();
    assert!(
        !executed.iter().any(|a| a.iter().any(|x| x == "venv")),
        "the venv is kept: {executed:?}"
    );
    let pip = executed
        .iter()
        .find(|a| a.iter().any(|x| x == "install"))
        .expect("pip install ran");
    assert!(
        pip.contains(&"litellm[proxy]==1.106.0".to_string()),
        "{pip:?}"
    );
    assert_eq!(ops.spawns().len(), 1);
}

#[tokio::test]
async fn start_gateway_skips_install_when_venv_litellm_matches_pin() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = fast_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        FakeSystemOps::ok_output(&format!("{}\n", crate::GATEWAY_PYTHON_VERSION)),
    );
    script_litellm_version(&ops, &step, &ctx, manager_config::DEFAULT_LITELLM_PIN);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_commands();
    assert!(
        executed.iter().all(|a| !a.iter().any(|x| x == "install")),
        "no pip install: {executed:?}"
    );
    let version_probes = executed
        .iter()
        .filter(|a| **a == litellm_version_argv(&step, &ctx))
        .count();
    assert_eq!(version_probes, 1, "the version is probed once");
}

/// A recording live sink for the logging tests: every line a
/// step emits while running lands here.
fn recording_sink() -> (crate::StepLogSink, Arc<std::sync::Mutex<Vec<String>>>) {
    let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = lines.clone();
    (
        Arc::new(move |line: String| captured.lock().unwrap().push(line)),
        lines,
    )
}

#[tokio::test]
async fn start_gateway_fast_path_logs_existing_install_verified() {
    // The healthy-start contract (field report 2026-10-19):
    // the step log states the verification and the skip, with
    // the probe evidence — and the same lines reach the live
    // sink WHILE the step runs (the wizard log is written from
    // the sink, so this is what the operator watches).
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let (sink, emitted) = recording_sink();
    let ctx = ctx_with(ops.clone(), RunMode::Install).with_log_sink(sink);
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        crate::CommandOutput {
            exit_code: 0,
            stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
            stderr: String::new(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("existing install verified — skipping install")),
        "{:?}",
        out.log_tail
    );
    let emitted = emitted.lock().unwrap();
    assert!(
        emitted
            .iter()
            .any(|l| l.contains("existing install verified — skipping install")),
        "live sink received the verification line: {emitted:?}"
    );
    assert!(
        emitted
            .iter()
            .any(|l| l.contains("waiting for the gateway to answer")),
        "live sink received the health-wait line: {emitted:?}"
    );
    assert!(
        emitted
            .iter()
            .any(|l| l.contains("final health re-check passed")),
        "live sink received the final re-check line: {emitted:?}"
    );
}

#[tokio::test]
async fn start_gateway_import_probe_failure_logs_error_and_repairs_packages_only() {
    // The reinstall-on-every-start defect: a probe failure used
    // to be a bare bool, so the log never said WHAT failed and
    // the repair looked identical to a full reinstall. The
    // import error itself must be logged BEFORE the repair,
    // and a package-level failure must not recreate the venv.
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let version_probe = step.version_probe_command(&ctx);
    let version_argv: Vec<&str> = version_probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &version_argv,
        crate::CommandOutput {
            exit_code: 0,
            stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
            stderr: String::new(),
        },
    );
    let import_probe = step.litellm_probe_command(&ctx);
    let import_argv: Vec<&str> = import_probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &import_argv,
        crate::CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "ModuleNotFoundError: No module named 'litellm'".to_string(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let repair_line = out
        .log_tail
        .iter()
        .position(|l| l.contains("litellm import probe failed"))
        .expect("the probe failure is logged");
    assert!(
        out.log_tail[repair_line].contains("ModuleNotFoundError"),
        "{}",
        out.log_tail[repair_line]
    );
    assert!(
        out.log_tail[repair_line].contains("installing packages only"),
        "{}",
        out.log_tail[repair_line]
    );
    // The repair line precedes the pip run's own log line.
    let pip_line = out
        .log_tail
        .iter()
        .position(|l| l.starts_with("running:") && l.contains("pip"))
        .expect("the pip repair is logged");
    assert!(repair_line < pip_line, "{:?}", out.log_tail);
    let executed = ops.executed_commands();
    assert!(
        executed.iter().any(|a| a.iter().any(|x| x == "install")),
        "pip install ran: {executed:?}"
    );
    assert!(
        executed.iter().all(|a| !a.iter().any(|x| x == "venv")),
        "the venv itself is kept: {executed:?}"
    );
}

#[tokio::test]
async fn start_gateway_tolerates_noisy_version_probe_output() {
    // Output pollution around the version line (an interpreter
    // warning, a sitecustomize banner) used to fail the
    // whole-stdout equality parse and force a full venv
    // rebuild on every start. Noise is not a different
    // version.
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &probe_argv,
        crate::CommandOutput {
            exit_code: 0,
            stdout: format!(
                "Warning: interpreter plugin failed to load\n{}\n",
                crate::GATEWAY_PYTHON_VERSION
            ),
            stderr: String::new(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("existing install verified — skipping install")),
        "{:?}",
        out.log_tail
    );
    let executed = ops.executed_commands();
    assert!(
        executed
            .iter()
            .all(|a| !a.iter().any(|x| x == "venv" || x == "install")),
        "no venv/pip commands may run: {executed:?}"
    );
}

#[tokio::test]
async fn start_gateway_fails_when_gateway_dies_after_health_checks() {
    // The false-"done" hole (field report 2026-10-19): health
    // passed, then the gateway died during the post-health
    // work (MCP check, firewall reconcile) — the step still
    // reported done. The final re-check must catch it.
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get_sequence(
        "http://127.0.0.1:4000/v1/models",
        vec![
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
            crate::HttpResponse {
                status: 500,
                body: Vec::new(),
                elapsed_ms: 0,
            },
        ],
    );
    ops.with_file(
        PathBuf::from("/data/logs/gateway.stderr.log"),
        b"litellm: fatal error after startup\n".to_vec(),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("did not stay up"), "{}", out.message);
    assert!(out.message.contains("HTTP 500"), "{}", out.message);
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("fatal error after startup")),
        "gateway output attached: {:?}",
        out.log_tail
    );
}

#[test]
fn version_parsers_tolerate_noise_and_reject_other_shapes() {
    assert_eq!(parse_two_part_version("3.12\n").as_deref(), Some("3.12"));
    assert_eq!(
        parse_two_part_version("Warning: noisy plugin\n3.12\n").as_deref(),
        Some("3.12")
    );
    assert_eq!(parse_two_part_version("3.12.1\n"), None);
    assert_eq!(parse_two_part_version("Python 3.12\n"), None);
    assert_eq!(parse_two_part_version(""), None);
    assert_eq!(
        parse_semver_version("1.105.1\n").as_deref(),
        Some("1.105.1")
    );
    assert_eq!(
        parse_semver_version("UserWarning: something\n0.11.0\n").as_deref(),
        Some("0.11.0")
    );
    assert_eq!(parse_semver_version("1.2\n"), None);
    assert_eq!(parse_semver_version("1.2.3.4\n"), None);
    assert_eq!(parse_semver_version("no version here\n"), None);
}

#[test]
fn progress_emitter_throttles_ticks() {
    let ops = Arc::new(FakeSystemOps::new());
    let (sink, emitted) = recording_sink();
    let ctx = ctx_with(ops, RunMode::Install).with_log_sink(sink);
    let emitter = ProgressEmitter::new(&ctx, "thing.zip");
    emitter.progress(0, 100); // first call always emits
    emitter.progress(5, 100); // <10-point step, <5 s: throttled
    emitter.progress(15, 100); // >=10-point step: emits
    emitter.progress(100, 100); // completion: emits
    let emitted = emitted.lock().unwrap();
    assert_eq!(emitted.len(), 3, "{emitted:?}");
    assert!(emitted[0].contains("(0%)"), "{}", emitted[0]);
    assert!(emitted[1].contains("(15%)"), "{}", emitted[1]);
    assert!(emitted[2].contains("(100%)"), "{}", emitted[2]);
}

#[tokio::test]
async fn start_gateway_spawn_uses_venv_python_and_captures_logs() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let cmds = ops.spawn_cmds();
    assert_eq!(cmds.len(), 1);
    // The venv interpreter directly, running the console
    // script's entry point via -c: never the litellm.exe launcher
    // (its child python.exe gets a visible console window) and
    // never `python -m litellm` (no __main__ in the package).
    assert!(
        cmds[0].argv[0].ends_with("python")
            || cmds[0].argv[0].ends_with("python.exe")
            || cmds[0].argv[0].ends_with("pythonw.exe"),
        "{:?}",
        cmds[0].argv
    );
    assert_eq!(cmds[0].argv[1], "-c");
    assert!(
        cmds[0].argv[2].contains("from litellm import run_server"),
        "{:?}",
        cmds[0].argv
    );
    assert!(
        !cmds[0].argv.iter().any(|a| a == "-m"),
        "{:?}",
        cmds[0].argv
    );
    // Output capture is wired to the gateway log files.
    assert_eq!(
        cmds[0].stdout_log.as_deref(),
        Some(std::path::Path::new("/data/logs/gateway.stdout.log"))
    );
    assert_eq!(
        cmds[0].stderr_log.as_deref(),
        Some(std::path::Path::new("/data/logs/gateway.stderr.log"))
    );
    // A master key was generated, stored, and injected into the
    // gateway's environment (current LiteLLM refuses to boot
    // without one).
    let key = cmds[0]
        .env
        .get("LITELLM_MASTER_KEY")
        .expect("master key env set");
    // UTF-8 stdio is forced: with redirected output, cp1252
    // cannot encode LiteLLM's startup banner and startup fails.
    assert_eq!(
        cmds[0].env.get("PYTHONIOENCODING").map(String::as_str),
        Some("utf-8")
    );
    assert_eq!(cmds[0].env.get("PYTHONUTF8").map(String::as_str), Some("1"));
    assert!(key.starts_with("sk-") && key.len() > 32, "{key}");
    assert_eq!(
        ops.credential_value(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .as_deref(),
        Some(key.as_str()),
        "stored credential matches the injected key"
    );
}

#[tokio::test]
async fn start_gateway_fails_fast_when_process_exits() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    // The spawned gateway exits immediately; its captured stderr
    // carries the interpreter error the operator needs to see.
    ops.with_dead_spawn();
    ops.with_file(
            PathBuf::from("/data/logs/gateway.stderr.log"),
            b"C:\\venv\\python.exe: No module named litellm.__main__; 'litellm' is a package and cannot be directly executed\n".to_vec(),
        );
    let step = StartGateway {
        health_retries: 30,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    // Detected on the first probe, not after the full budget.
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("exited during startup (probe attempt 1)")),
        "{out:?}"
    );
    // The captured gateway stderr tail rides in the outcome.
    assert!(
        out.log_tail.iter().any(|l| l.contains("No module named")),
        "{out:?}"
    );
}

#[tokio::test]
async fn start_gateway_install_stops_previous_and_records_pid() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    // A previous gateway (pid 7777) is recorded and still running.
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // The old process was terminated before the replacement spawned,
    // and the pid file now records the new spawn (fake pid 4242).
    assert_eq!(ops.killed_pids(), vec![7777]);
    assert_eq!(ops.spawns().len(), 1);
    let pid_file = ops
        .file_bytes(&PathBuf::from("/data/gateway.pid"))
        .expect("pid file written");
    assert_eq!(String::from_utf8(pid_file).unwrap(), "4242");
}

/// PL-M4 (docs/code-review-v0.1.60.md): a recorded gateway pid
/// that now belongs to another program (pid reuse after a reboot)
/// is treated as stale, never force-killed.
#[tokio::test]
async fn start_gateway_does_not_kill_a_recycled_gateway_pid() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    ops.with_process_image(7777, "explorer.exe");
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops.killed_pids().is_empty(), "{:?}", ops.killed_pids());
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("belongs to another program")),
        "{out:?}"
    );
    let pid_file = ops
        .file_bytes(&PathBuf::from("/data/gateway.pid"))
        .expect("pid file written");
    assert_eq!(String::from_utf8(pid_file).unwrap(), "4242");
}

/// PL-M4: the recorded gateway (image python.exe) is still stopped.
#[tokio::test]
async fn start_gateway_stops_previous_gateway_with_matching_image() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    ops.with_process_image(7777, "python.exe");
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.killed_pids(), vec![7777]);
}

#[tokio::test]
async fn start_gateway_install_fails_when_previous_cannot_be_stopped() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    ops.with_kill_failure();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(
        out.message.contains("could not stop the previous gateway"),
        "{out:?}"
    );
    // No duplicate was spawned over the still-running old process.
    assert!(ops.spawns().is_empty());
}

#[tokio::test]
async fn start_gateway_audit_does_not_stop_recorded_gateway() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok("{}"),
    );
    ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
    ops.with_live_pid(7777);
    let ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = StartGateway::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);
    assert!(ops.killed_pids().is_empty(), "audit kills nothing");
    assert!(ops.spawns().is_empty(), "audit spawns nothing");
}

fn gateway_ctx_with_openrouter(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
    let mut cfg = test_config_with_models();
    cfg.gateway.openrouter.enabled = true;
    cfg.gateway.openrouter.cred_ref = Some("LocalLLM/OpenRouter".to_string());
    cfg.gateway.openrouter.model_allowlist = vec!["openai/gpt-4o-mini".to_string()];
    WizardContext::new(cfg, PathBuf::from("/data"), mode, ops)
}

/// Script the bootstrapped uv in the fake fs so gateway tests do
/// not need the PATH-mutating PathGuard (parallel tests share the
/// process PATH; these tests must not race the ones that use it).
pub(super) fn with_bundled_uv(ops: &FakeSystemOps) {
    let name = if cfg!(target_os = "windows") {
        "uv.exe"
    } else {
        "uv"
    };
    ops.with_file(PathBuf::from("/data/bin").join(name), b"fake-uv".to_vec());
}

#[tokio::test]
async fn start_gateway_injects_openrouter_key_from_credential() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_credential_value("LocalLLM/OpenRouter", "sk-test-value");
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = gateway_ctx_with_openrouter(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    // The plan records the variable but never the value.
    let planned = step.planned_commands(&ctx);
    let plan_spawn = planned
        .iter()
        .find(|c| c.argv.iter().any(|a| a == "--config"))
        .expect("gateway spawn planned");
    assert_eq!(
        plan_spawn.env.get("OPENROUTER_API_KEY").map(String::as_str),
        Some("<set from credential manager>")
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // The spawned gateway carries the key in its environment.
    let envs = ops.spawn_envs();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].get("OPENROUTER_API_KEY").map(String::as_str),
        Some("sk-test-value")
    );
    // The configured bind on the spawn argv: the shipped default
    // is 0.0.0.0 (LAN-reachable), per Pete's 2026-10-15 decision.
    let spawns = ops.spawns();
    let host = spawns[0]
        .windows(2)
        .find(|w| w[0] == "--host")
        .expect("--host present");
    assert_eq!(host[1], "0.0.0.0");
    // The key never lands in the generated YAML.
    let cfg = ops
        .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
        .expect("config written");
    let text = String::from_utf8(cfg).unwrap();
    assert!(!text.contains("sk-test-value"), "{text}");
}

/// Firewall reconciliation, add path: LAN-bound (the default)
/// with no rule present — the step elevates once to run the add
/// script, and a successful add leaves the Done message clean.
#[tokio::test]
async fn start_gateway_adds_firewall_rule_when_missing() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_command_failure(
        &[
            "netsh",
            "advfirewall",
            "firewall",
            "show",
            "rule",
            "name=Local LLM Gateway (TCP 4000)",
        ],
        1,
        "No rules match the specified criteria.",
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(!out.message.contains("firewall"), "{out:?}");
    let elevated = ops.elevated_calls();
    assert_eq!(elevated.len(), 1, "{elevated:?}");
    let script = ops
        .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
        .expect("firewall script written");
    let text = String::from_utf8(script).unwrap();
    assert!(
        text.contains("add rule name=\"Local LLM Gateway (TCP 4000)\""),
        "{text}"
    );
    assert!(text.contains("localport=4000"), "{text}");
    assert!(text.contains("remoteip=localsubnet"), "{text}");
}

/// A17: a start on a new port removes this app's rules for earlier
/// ports in the same elevated script, leaves every other rule alone,
/// and says beforehand that Windows will ask for approval.
#[tokio::test]
async fn start_gateway_on_a_new_port_removes_the_old_ports_rule() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4001/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let show = |name: &str| {
        ["netsh", "advfirewall", "firewall", "show", "rule", name]
            .map(str::to_string)
            .to_vec()
    };
    let missing = show("name=Local LLM Gateway (TCP 4001)");
    ops.with_command_failure(
        &missing.iter().map(String::as_str).collect::<Vec<_>>(),
        1,
        "No rules match the specified criteria.",
    );
    // What this data directory opened before; 4002's rule is gone.
    let gone = show("name=Local LLM Gateway (TCP 4002)");
    ops.with_command_failure(
        &gone.iter().map(String::as_str).collect::<Vec<_>>(),
        1,
        "No rules match the specified criteria.",
    );
    ops.with_file(
        PathBuf::from("/data/gateway/firewall.cmd"),
        b"netsh advfirewall firewall delete rule name=\"Local LLM Gateway (TCP 4002)\"\r\n\
          netsh advfirewall firewall add rule name=\"Local LLM Gateway (TCP 4000)\" dir=in\r\n"
            .to_vec(),
    );
    let mut config = test_config_with_models();
    config.gateway.port = 4001;
    let ctx = WizardContext::new(
        config,
        PathBuf::from("/data"),
        RunMode::Install,
        ops.clone(),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.elevated_calls().len(), 1);
    let script = ops
        .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
        .expect("firewall script written");
    let text = String::from_utf8(script).unwrap();
    for port in [4000, 4001] {
        assert!(
            text.contains(&format!(
                "delete rule name=\"Local LLM Gateway (TCP {port})\""
            )),
            "{text}"
        );
    }
    assert!(!text.contains("4002"), "{text}");
    assert!(
        text.contains("add rule name=\"Local LLM Gateway (TCP 4001)\""),
        "{text}"
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("administrator approval")),
        "{out:?}"
    );
}

/// Firewall reconciliation, remove path: loopback-bound with the
/// rule present (the fake's default command success) — one
/// elevated delete.
#[tokio::test]
async fn start_gateway_removes_firewall_rule_when_loopback() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let mut config = test_config_with_models();
    config.gateway.bind_host = "127.0.0.1".to_string();
    let ctx = WizardContext::new(
        config,
        PathBuf::from("/data"),
        RunMode::Install,
        ops.clone(),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.elevated_calls().len(), 1);
    let script = ops
        .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
        .expect("firewall script written");
    let text = String::from_utf8(script).unwrap();
    assert!(
        text.contains("delete rule name=\"Local LLM Gateway (TCP 4000)\""),
        "{text}"
    );
    assert!(!text.contains("add rule"), "{text}");
}

/// Firewall reconciliation, no-op path: LAN-bound with the rule
/// already present — no elevation, no script.
#[tokio::test]
async fn start_gateway_firewall_noop_when_rule_matches() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops.elevated_calls().is_empty());
    assert!(ops
        .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
        .is_none());
}

/// A17, the way back: the current port's rule exists, an earlier
/// port's is left over. Only that one is removed, the current rule is
/// not touched, and a declined prompt is not repeated.
#[tokio::test]
async fn start_gateway_removes_a_leftover_rule_once() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_file(
        PathBuf::from("/data/gateway/firewall.cmd"),
        b"netsh advfirewall firewall add rule name=\"Local LLM Gateway (TCP 4001)\" dir=in\r\n"
            .to_vec(),
    );
    ops.with_elevated_error("declined");
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    for _ in 0..2 {
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert_eq!(ops.elevated_calls().len(), 1);
    }
    let text = String::from_utf8(
        ops.file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
            .unwrap(),
    )
    .unwrap();
    assert!(
        text.contains("delete rule name=\"Local LLM Gateway (TCP 4001)\""),
        "{text}"
    );
    assert!(!text.contains("4000"), "{text}");
}

#[test]
fn gateway_probe_host_maps_wildcards_to_loopback() {
    let mut config = test_config_with_models();
    assert_eq!(gateway_probe_host(&config), "127.0.0.1");
    config.gateway.bind_host = "::".to_string();
    assert_eq!(gateway_probe_host(&config), "127.0.0.1");
    config.gateway.bind_host = "127.0.0.1".to_string();
    assert_eq!(gateway_probe_host(&config), "127.0.0.1");
    config.gateway.bind_host = "192.168.1.50".to_string();
    assert_eq!(gateway_probe_host(&config), "192.168.1.50");
}

#[tokio::test]
async fn start_gateway_warns_when_openrouter_credential_unreadable() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    // No credential scripted: credential_read returns None.
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = gateway_ctx_with_openrouter(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    // The step still succeeds; the outcome carries the warning and
    // the spawn has no key.
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message.contains("OpenRouter fallback is not armed"),
        "{out:?}"
    );
    let envs = ops.spawn_envs();
    assert_eq!(envs.len(), 1);
    assert!(!envs[0].contains_key("OPENROUTER_API_KEY"));
}

#[tokio::test]
async fn start_gateway_fails_when_unhealthy() {
    let _guard = PathGuard::set_to(&path_dir_with_uv(true));
    let ops = Arc::new(FakeSystemOps::new());
    // No scripted health response → probes fail.
    let ctx = ctx_with(ops, RunMode::Install);
    let step = StartGateway {
        health_retries: 2,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("did not become healthy"));
}

#[tokio::test]
async fn start_gateway_failure_carries_manual_start_block() {
    let _guard = PathGuard::set_to(&path_dir_with_uv(true));
    let ops = Arc::new(FakeSystemOps::new());
    // No scripted health response → probes fail.
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = StartGateway {
        health_retries: 2,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    let manual = out
        .manual_command
        .expect("a failed start carries the manual start block");
    // The spawn command exactly as the step builds it: the
    // bootstrap entry point, this install's config path, port.
    assert!(manual.contains("run_server"), "{manual}");
    assert!(manual.contains("--config"), "{manual}");
    assert!(manual.contains("config.yaml"), "{manual}");
    assert!(manual.contains("4000"), "{manual}");
    // The spawn line opens with the PowerShell call operator —
    // without it a quoted executable path is a parse error.
    let spawn_line = manual
        .lines()
        .find(|l| l.contains("run_server"))
        .expect("manual block carries the spawn line");
    assert!(spawn_line.starts_with("& "), "{manual}");
    // The master key is named with its Credential Manager
    // target, and the stored value never appears in the block.
    assert!(manual.contains("LITELLM_MASTER_KEY"), "{manual}");
    assert!(
        manual.contains(crate::LITELLM_MASTER_KEY_CRED_TARGET),
        "{manual}"
    );
    let stored = ops
        .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
        .await
        .expect("the step stored a master key");
    assert!(
        !manual.contains(&stored),
        "manual block leaked the master key value"
    );
    // PostgreSQL is not installed in the fake fs: no pg_ctl
    // pre-step and no DATABASE_URL line.
    assert!(!manual.contains("pg_ctl"), "{manual}");
    assert!(!manual.contains("DATABASE_URL"), "{manual}");
    assert!(
        !manual.contains("PRISMA_HEALTH_WATCHDOG_ENABLED"),
        "{manual}"
    );
}

#[tokio::test]
async fn manual_start_block_includes_postgres_pre_step_when_installed() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    ops.with_file(
        postgres_bin_dir(&ctx.data_dir).join("pg_ctl.exe"),
        Vec::new(),
    );
    ops.with_file(postgres_marker_path(&ctx.data_dir), b"{}".to_vec());
    let block = StartGateway::default().manual_start_block(&ctx).await;
    assert!(block.contains("pg_ctl"), "{block}");
    assert!(
        block.contains(&format!(
            "postgresql://postgres:<password>@127.0.0.1:{}/litellm",
            ctx.config.postgres.port
        )),
        "{block}"
    );
    assert!(!block.contains("<port>"), "no placeholder port: {block}");
    assert!(
        block.contains(crate::POSTGRES_PASSWORD_CRED_TARGET),
        "{block}"
    );
    // The watchdog disable rides with the DATABASE_URL line:
    // a manual start with a database needs it on Windows or
    // the gateway dies about a second after startup.
    let db_at = block.find("DATABASE_URL").unwrap();
    let watchdog_at = block
        .find("PRISMA_HEALTH_WATCHDOG_ENABLED")
        .expect("manual block carries the watchdog disable");
    assert!(db_at < watchdog_at, "{block}");
    // The pg_ctl pre-step precedes the gateway spawn line.
    let pg_at = block.find("pg_ctl").unwrap();
    let spawn_at = block.find("run_server").unwrap();
    assert!(pg_at < spawn_at, "{block}");
    assert!(watchdog_at < spawn_at, "{block}");
    // Both executable lines open with the PowerShell call
    // operator; the $env: lines between them do not.
    let pg_line = block
        .lines()
        .find(|l| l.contains("pg_ctl"))
        .expect("manual block carries the pg_ctl line");
    assert!(pg_line.starts_with("& "), "{block}");
    let spawn_line = block
        .lines()
        .find(|l| l.contains("run_server"))
        .expect("manual block carries the spawn line");
    assert!(spawn_line.starts_with("& "), "{block}");
}
