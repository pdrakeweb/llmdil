use super::*;

fn pg_pin_for(zip: &[u8]) -> PostgresPin {
    PostgresPin {
        version: "18.6-1".to_string(),
        asset: "postgresql-18.6-1-windows-x64-binaries.zip".to_string(),
        sha256: sha256_hex(zip),
        size: zip.len() as u64,
    }
}

fn stage_pg_binaries(ops: &FakeSystemOps) {
    for exe in [
        "pg_ctl.exe",
        "initdb.exe",
        "postgres.exe",
        "psql.exe",
        "createdb.exe",
    ] {
        ops.with_extracted_file(
            PathBuf::from("/data/postgres/bin").join(exe),
            b"fake-pg-binary".to_vec(),
        );
    }
}

#[tokio::test]
async fn install_postgres_downloads_extracts_initializes_and_creates_db() {
    let ops = Arc::new(FakeSystemOps::new());
    let zip = b"fake-pg-zip";
    let pin = pg_pin_for(zip);
    ops.with_download(&pin.url(), zip.to_vec());
    stage_pg_binaries(&ops);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallPostgres::with_pin(pin.clone())
        .run(&ctx)
        .await
        .unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message.contains("running on 127.0.0.1:54329"),
        "{}",
        out.message
    );
    assert_eq!(ops.downloaded_to().len(), 1);
    let extractions = ops.zip_prefix_extractions();
    assert_eq!(extractions.len(), 1);
    assert_eq!(
        extractions[0].2,
        vec![
            "pgsql/bin/".to_string(),
            "pgsql/lib/".to_string(),
            "pgsql/share/".to_string(),
            "pgsql/include/".to_string()
        ]
    );
    assert_eq!(extractions[0].3, "pgsql/");
    // initdb ran with scram auth; createdb made the litellm db.
    let executed = ops.executed_commands();
    let initdb = executed
        .iter()
        .find(|a| a.iter().any(|x| x.ends_with("initdb.exe")))
        .expect("initdb ran");
    assert!(initdb.iter().any(|x| x == "scram-sha-256"), "{initdb:?}");
    let createdb = executed
        .iter()
        .find(|a| a.iter().any(|x| x.ends_with("createdb.exe")))
        .expect("createdb ran");
    assert_eq!(createdb.last().map(String::as_str), Some("litellm"));
    // The password file never survives the step.
    assert!(ops
        .file_bytes(&PathBuf::from("/data/postgres/pwfile.tmp"))
        .is_none());
    // The generated password is stored for the gateway spawns.
    let stored = ops
        .credential_value(crate::POSTGRES_PASSWORD_CRED_TARGET)
        .expect("postgres password stored");
    assert_eq!(stored.len(), 64, "hex password: {stored}");
    let marker = ops
        .file_bytes(&PathBuf::from("/data/postgres/.installed.json"))
        .expect("marker written");
    let v: serde_json::Value = serde_json::from_slice(&marker).unwrap();
    assert_eq!(v["database"], "litellm");
    assert_eq!(v["port"], 54329);
}

#[tokio::test]
async fn install_postgres_skips_extraction_without_marker_when_cluster_present() {
    // Pete's machine, 2026-10-16: a working install whose
    // .installed.json marker was lost. Binaries + initialized
    // cluster are the ground truth — the step must not
    // re-extract over the running server's DLLs; it rewrites
    // the marker and completes.
    let ops = Arc::new(FakeSystemOps::new());
    let pin = pg_pin_for(b"unused");
    for exe in [
        "pg_ctl.exe",
        "initdb.exe",
        "postgres.exe",
        "psql.exe",
        "createdb.exe",
    ] {
        ops.with_file(PathBuf::from("/data/postgres/bin").join(exe), b"x".to_vec());
    }
    ops.with_file(
        PathBuf::from("/data/postgres/data/PG_VERSION"),
        b"18\n".to_vec(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops.zip_prefix_extractions().is_empty());
    assert!(ops.downloaded_to().is_empty());
    assert!(
        ops.file_bytes(&PathBuf::from("/data/postgres/.installed.json"))
            .is_some(),
        "marker rewritten"
    );
}

#[tokio::test]
async fn install_postgres_stops_running_server_before_extract() {
    // Re-extraction IS required (no cluster yet), and the fake
    // reports the server ready (its run_command succeeds for
    // pg_isready): the step must issue pg_ctl stop before
    // extracting, or the real extraction dies on locked DLLs.
    let ops = Arc::new(FakeSystemOps::new());
    let zip = b"fake-pg-zip";
    let pin = pg_pin_for(zip);
    ops.with_download(&pin.url(), zip.to_vec());
    stage_pg_binaries(&ops);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.zip_prefix_extractions().len(), 1);
    let executed = ops.executed_commands();
    let stop = executed
        .iter()
        .find(|a| a.iter().any(|x| x.ends_with("pg_ctl.exe")) && a.iter().any(|x| x == "stop"))
        .expect("pg_ctl stop issued before extraction");
    assert!(stop.iter().any(|x| x == "fast"), "{stop:?}");
}

#[tokio::test]
async fn install_postgres_checksum_mismatch_fails_closed() {
    let ops = Arc::new(FakeSystemOps::new());
    let pin = pg_pin_for(b"the-real-zip");
    ops.with_download(&pin.url(), b"tampered-bytes".to_vec());
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("checksum mismatch"), "{}", out.message);
    assert!(ops.zip_prefix_extractions().is_empty());
}

#[tokio::test]
async fn install_postgres_audit_requires_install_and_cluster() {
    let ops = Arc::new(FakeSystemOps::new());
    let pin = pg_pin_for(b"unused");
    let ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = InstallPostgres::with_pin(pin.clone())
        .run(&ctx)
        .await
        .unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("not installed"), "{}", out.message);
    // Fully staged install passes the audit without running
    // anything.
    for exe in [
        "pg_ctl.exe",
        "initdb.exe",
        "postgres.exe",
        "psql.exe",
        "createdb.exe",
    ] {
        ops.with_file(PathBuf::from("/data/postgres/bin").join(exe), b"x".to_vec());
    }
    ops.with_file(
        PathBuf::from("/data/postgres/.installed.json"),
        b"{}".to_vec(),
    );
    ops.with_file(
        PathBuf::from("/data/postgres/data/PG_VERSION"),
        b"18\n".to_vec(),
    );
    let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops.executed_commands().is_empty());
}

/// argv of the `pg_isready` probe `postgres_ready` runs for a
/// data dir, for scripting in the ensure-postgres tests below.
pub(super) fn pg_isready_argv(data_dir: &Path) -> Vec<String> {
    vec![
        postgres_bin_dir(data_dir)
            .join("pg_isready.exe")
            .display()
            .to_string(),
        "-h".to_string(),
        "127.0.0.1".to_string(),
        "-p".to_string(),
        "54329".to_string(),
    ]
}

pub(super) fn argv_refs(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

pub(super) fn pg_not_ready_output() -> CommandOutput {
    CommandOutput {
        exit_code: 1,
        stdout: String::new(),
        stderr: "no response".to_string(),
    }
}

pub(super) fn pg_ready_output() -> CommandOutput {
    CommandOutput {
        exit_code: 0,
        stdout: "127.0.0.1:54329 - accepting connections".to_string(),
        stderr: String::new(),
    }
}

/// Context for the free postgres functions in the tests
/// below: they take the context for its config, data dir, ops
/// and live sink.
pub(super) fn pg_ctx(ops: &Arc<FakeSystemOps>, cfg: AppConfig, data_dir: &Path) -> WizardContext {
    WizardContext::new(cfg, data_dir.to_path_buf(), RunMode::Install, ops.clone())
}

#[tokio::test]
async fn postgres_start_adopts_live_postmaster_not_yet_ready() {
    // A postmaster from an earlier session is still starting
    // (pid file present, pid live and a postgres image, probe
    // not answering yet): it is awaited, never competed with.
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    ops.with_file(postmaster_pid_path(&data_dir), b"4242\n/data\n".to_vec());
    ops.with_process_image(4242, "postgres.exe");
    let isready = pg_isready_argv(&data_dir);
    ops.with_command_sequence(
        &argv_refs(&isready),
        vec![pg_not_ready_output(), pg_ready_output()],
    );
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    let url = ensure_postgres_running(&ctx, &mut log)
        .await
        .expect("adopts the running postmaster");
    assert!(url.contains("127.0.0.1:54329"), "{url}");
    assert!(log.iter().any(|l| l.contains("waiting for it")), "{log:?}");
    assert!(
        !ops.executed_commands()
            .iter()
            .any(|a| a.iter().any(|x| x == "start")),
        "no competing pg_ctl start: {:?}",
        ops.executed_commands()
    );
}

#[tokio::test]
async fn postgres_start_removes_stale_pid_file_with_dead_pid() {
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let pid_path = postmaster_pid_path(&data_dir);
    ops.with_file(pid_path.clone(), b"4242\n".to_vec());
    // 4242 is not scripted live: the recorded postmaster is gone.
    let isready = pg_isready_argv(&data_dir);
    ops.with_command_sequence(
        &argv_refs(&isready),
        vec![pg_not_ready_output(), pg_ready_output()],
    );
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    ensure_postgres_running(&ctx, &mut log)
        .await
        .expect("starts after clearing the stale pid file");
    assert!(
        ops.file_bytes(&pid_path).is_none(),
        "stale pid file removed"
    );
    let start = postgres_start_command(&data_dir, 54329);
    assert!(
        ops.executed_commands().contains(&start.argv),
        "pg_ctl start ran: {:?}",
        ops.executed_commands()
    );
    // The start command launches without waiting: readiness is
    // polled by the caller, not by pg_ctl's own wait.
    assert!(
        start.argv.iter().any(|a| a == "--no-wait"),
        "{:?}",
        start.argv
    );
    assert!(!start.argv.iter().any(|a| a == "-t"), "{:?}", start.argv);
}

#[tokio::test]
async fn postgres_start_removes_pid_file_recycled_by_another_program() {
    // The recorded pid is alive but belongs to an unrelated
    // program now (Windows pid reuse): the pid file is stale
    // even though the pid is not dead.
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let pid_path = postmaster_pid_path(&data_dir);
    ops.with_file(pid_path.clone(), b"4242\n".to_vec());
    ops.with_process_image(4242, "explorer.exe");
    let isready = pg_isready_argv(&data_dir);
    ops.with_command_sequence(
        &argv_refs(&isready),
        vec![pg_not_ready_output(), pg_ready_output()],
    );
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    ensure_postgres_running(&ctx, &mut log)
        .await
        .expect("starts after clearing the recycled pid file");
    assert!(
        ops.file_bytes(&pid_path).is_none(),
        "recycled pid file removed"
    );
    let start = postgres_start_command(&data_dir, 54329);
    assert!(ops.executed_commands().contains(&start.argv));
}

#[tokio::test]
async fn postgres_start_failure_carries_log_tail_when_pg_ctl_is_stopped() {
    // The 2026-10-18 field failure shape: the pg_ctl command
    // itself is stopped by a timeout (a WizardError, not an
    // exit code). The failure must carry the server log tail —
    // the old code attached it only to non-zero exits, leaving
    // exactly this path undiagnosable.
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let isready = pg_isready_argv(&data_dir);
    ops.with_command(&argv_refs(&isready), pg_not_ready_output());
    let start = postgres_start_command(&data_dir, 54329);
    ops.with_command_failure(
        &argv_refs(&start.argv),
        -1,
        "timed out after 60s and was stopped",
    );
    ops.with_file(
        postgres_log_path(&data_dir),
        b"2026-10-18 FATAL: the database system is starting up\nrecovery is in progress\n".to_vec(),
    );
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    let err = ensure_postgres_running(&ctx, &mut log)
        .await
        .expect_err("the start must fail");
    assert!(err.contains("timed out after 60s"), "{err}");
    assert!(err.contains("server log tail"), "{err}");
    assert!(err.contains("the database system is starting up"), "{err}");
}

#[tokio::test]
async fn postgres_nowait_launch_polls_to_ready_and_reports_elapsed() {
    // The 2026-11-02 shape, inverted: pg_ctl launches without
    // waiting and the caller polls to ready. The launch command
    // carries --no-wait and no -t; the log shows a launch
    // acceptance line and then a measured ready line — never
    // the misleading "command failed … timed out" line the old
    // waiter shape produced on the recovered path.
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    // A dead pid file: the stale block removes it and the flow
    // reaches the pg_ctl launch (a live pid would take the adopt
    // path and never run pg_ctl). The grace then proceeds via
    // the ready probe, and the settle poll confirms readiness.
    let pid_path = postmaster_pid_path(&data_dir);
    ops.with_file(pid_path, b"4242\n".to_vec());
    let isready = pg_isready_argv(&data_dir);
    ops.with_command_sequence(
        &argv_refs(&isready),
        vec![pg_not_ready_output(), pg_ready_output()],
    );
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    let url = ensure_postgres_running(&ctx, &mut log)
        .await
        .expect("the launch must poll to ready");
    assert!(
        url.contains("127.0.0.1:54329/litellm"),
        "the start returns the LiteLLM database URL: {url}"
    );
    let start = postgres_start_command(&data_dir, 54329);
    assert!(
        ops.executed_commands().contains(&start.argv),
        "the launch command must run"
    );
    assert!(
        start.argv.iter().any(|a| a == "--no-wait"),
        "{:?}",
        start.argv
    );
    assert!(log.iter().any(|l| l.contains("launch accepted")), "{log:?}");
    assert!(
        log.iter()
            .any(|l| l.contains("PostgreSQL ready on 127.0.0.1:54329 after")),
        "{log:?}"
    );
    assert!(!log.iter().any(|l| l.contains("command failed")), "{log:?}");
}

#[tokio::test]
async fn postgres_nowait_launch_without_postmaster_fails_after_grace() {
    // A launch that reports success but produces no postmaster
    // (no pid file, no ready probe) fails at the end of the pid
    // grace — with the log tail — instead of burning the whole
    // settle window. Costs ~10 s of real sleeps (the grace).
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let isready = pg_isready_argv(&data_dir);
    ops.with_command(&argv_refs(&isready), pg_not_ready_output());
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    let err = ensure_postgres_running(&ctx, &mut log)
        .await
        .expect_err("a launch with no postmaster must fail");
    assert!(err.contains("no postmaster appeared"), "{err}");
    assert!(err.contains("server log tail"), "{err}");
}

#[test]
#[cfg(windows)]
fn gateway_python_is_console_interpreter_on_windows() {
    // Regression 2026-11-02: pythonw (GUI subsystem) cannot own
    // a console, so its console children (the Prisma query
    // engine) each opened a visible console window. python.exe
    // with CREATE_NO_WINDOW alone gives a hidden console that
    // children inherit.
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = ctx_with(ops, RunMode::Install);
    let step = StartGateway::default();
    let python = step.gateway_python(&ctx);
    let name = python
        .file_name()
        .expect("an interpreter path")
        .to_string_lossy();
    assert_eq!(name, "python.exe", "{}", python.display());
}

#[tokio::test]
async fn postgres_start_pg_ctl_failure_carries_log_tail() {
    // pg_ctl exits non-zero at once (the postmaster FATALed,
    // e.g. on the lock file): the settle poll sees no postmaster
    // and returns promptly; the failure carries pg_ctl's report
    // plus the server log tail.
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let isready = pg_isready_argv(&data_dir);
    ops.with_command(&argv_refs(&isready), pg_not_ready_output());
    let start = postgres_start_command(&data_dir, 54329);
    ops.with_command(
        &argv_refs(&start.argv),
        CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "could not start server".to_string(),
        },
    );
    ops.with_file(
        postgres_log_path(&data_dir),
        b"FATAL: lock file \"postmaster.pid\" already exists\n".to_vec(),
    );
    let cfg = test_config_with_models();
    let ctx = pg_ctx(&ops, cfg, &data_dir);
    let mut log = Vec::new();
    let err = ensure_postgres_running(&ctx, &mut log)
        .await
        .expect_err("the start must fail");
    assert!(err.contains("pg_ctl exit 1"), "{err}");
    assert!(err.contains("server log tail"), "{err}");
    assert!(err.contains("lock file"), "{err}");
}

#[tokio::test]
async fn postgres_wait_reports_gone_ready_and_timeout() {
    let data_dir = PathBuf::from("/data");
    let cfg = test_config_with_models();
    let isready = pg_isready_argv(&data_dir);
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command(&argv_refs(&isready), pg_not_ready_output());
    let ctx = pg_ctx(&ops, cfg.clone(), &data_dir);
    let mut log = Vec::new();
    // Gone: a postmaster is required but no pid file exists.
    assert_eq!(
        wait_for_postgres_ready(&ctx, &data_dir, POSTGRES_SETTLE_WAIT, true, &mut log).await,
        PostgresWait::PostmasterGone
    );
    // TimedOut: no postmaster requirement, never ready, and a
    // zero window — returns without sleeping.
    assert_eq!(
        wait_for_postgres_ready(&ctx, &data_dir, std::time::Duration::ZERO, false, &mut log).await,
        PostgresWait::TimedOut
    );
    // Ready: the probe answers on the second call.
    let ops2 = Arc::new(FakeSystemOps::new());
    ops2.with_command_sequence(
        &argv_refs(&isready),
        vec![pg_not_ready_output(), pg_ready_output()],
    );
    let ctx2 = pg_ctx(&ops2, cfg, &data_dir);
    assert_eq!(
        wait_for_postgres_ready(&ctx2, &data_dir, POSTGRES_SETTLE_WAIT, false, &mut log).await,
        PostgresWait::Ready
    );
}

#[tokio::test]
async fn postgres_log_streamer_emits_only_appended_lines() {
    // The readiness waits stream the postmaster's own log into
    // the step log: lines already present when the wait starts
    // are history (the failure tail covers them); lines
    // appended during the wait are this start's news and must
    // surface as they land.
    let ops = Arc::new(FakeSystemOps::new());
    let data_dir = PathBuf::from("/data");
    let log_path = postgres_log_path(&data_dir);
    ops.with_file(log_path.clone(), b"l1\nl2\nl3\n".to_vec());
    let ctx = pg_ctx(&ops, test_config_with_models(), &data_dir);
    let mut streamer = FileLineStreamer::new();
    streamer.prime(ops.as_ref(), &log_path).await;
    let mut log: Vec<String> = Vec::new();
    streamer
        .stream_new(&ctx, ops.as_ref(), &log_path, "postgres: ", &mut log)
        .await;
    assert!(log.is_empty(), "primed lines are not replayed: {log:?}");
    // The postmaster appends two lines (a FATAL among them).
    ops.with_file(
        log_path.clone(),
        b"l1\nl2\nl3\nl4\nFATAL: could not open lock file\n".to_vec(),
    );
    streamer
        .stream_new(&ctx, ops.as_ref(), &log_path, "postgres: ", &mut log)
        .await;
    assert_eq!(
        log,
        vec![
            "postgres: l4".to_string(),
            "postgres: FATAL: could not open lock file".to_string()
        ]
    );
    streamer
        .stream_new(&ctx, ops.as_ref(), &log_path, "postgres: ", &mut log)
        .await;
    assert_eq!(log.len(), 2, "no line is emitted twice: {log:?}");
}

#[tokio::test]
async fn postgres_wait_ticks_report_elapsed_on_the_live_sink() {
    // With the tick set to every poll, a two-poll wait reports
    // itself twice on the live sink — the production cadence
    // (~10 s) is the same code with a larger tick.
    let data_dir = PathBuf::from("/data");
    let cfg = test_config_with_models();
    let isready = pg_isready_argv(&data_dir);
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command_sequence(
        &argv_refs(&isready),
        vec![
            pg_not_ready_output(),
            pg_not_ready_output(),
            pg_ready_output(),
        ],
    );
    let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = lines.clone();
    let ctx = pg_ctx(&ops, cfg, &data_dir).with_log_sink(Arc::new(move |line: String| {
        captured.lock().unwrap().push(line)
    }));
    let mut log = Vec::new();
    let outcome =
        wait_for_postgres_ready_ticked(&ctx, &data_dir, POSTGRES_SETTLE_WAIT, false, &mut log, 1)
            .await;
    assert_eq!(outcome, PostgresWait::Ready);
    let lines = lines.lock().unwrap();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("waiting for PostgreSQL on 127.0.0.1:54329"))
            .count(),
        2,
        "{lines:?}"
    );
}

#[tokio::test]
async fn start_gateway_passes_database_url_when_postgres_installed() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_file(
        PathBuf::from("/data/postgres/.installed.json"),
        b"{}".to_vec(),
    );
    ops.with_file(
        PathBuf::from("/data/postgres/bin/pg_ctl.exe"),
        b"fake".to_vec(),
    );
    ops.with_credential_value(crate::POSTGRES_PASSWORD_CRED_TARGET, "deadbeefcafe");
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
    let envs = ops.spawn_envs();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].get("DATABASE_URL").map(String::as_str),
        Some("postgresql://postgres:deadbeefcafe@127.0.0.1:54329/litellm")
    );
    // Without the install, the gateway spawns database-free.
    let ops2 = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops2);
    ops2.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
    let out2 = step.run(&ctx2).await.unwrap();
    assert_eq!(out2.state, StepState::Done, "{out2:?}");
    assert!(!ops2.spawn_envs()[0].contains_key("DATABASE_URL"));
}

#[tokio::test]
async fn install_mxc_declined_elevation_completes_with_prep_outstanding() {
    let ops = Arc::new(FakeSystemOps::new());
    let exec = b"fake-wxc-exec";
    let prep = b"fake-wxc-host-prep";
    let pin = mxc_pin_for(b"unused-tarball", exec, prep);
    ops.with_file(
        PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
        exec.to_vec(),
    );
    ops.with_file(
        PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
        prep.to_vec(),
    );
    ops.with_elevated_error("elevation failed or was declined (ShellExecuteEx error 1223)");
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message.contains("host preparation was not completed"),
        "{}",
        out.message
    );
    let marker = ops
        .file_bytes(&PathBuf::from("/data/mxc/.installed.json"))
        .expect("marker written");
    let v: serde_json::Value = serde_json::from_slice(&marker).unwrap();
    assert_eq!(v["host_prep"]["system_drive"], false);
}

#[tokio::test]
async fn install_mxc_audit_requires_install_then_verifies() {
    let ops = Arc::new(FakeSystemOps::new());
    let exec = b"fake-wxc-exec";
    let prep = b"fake-wxc-host-prep";
    let pin = mxc_pin_for(b"unused-tarball", exec, prep);
    let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = InstallMxc::with_pin(pin.clone())
        .run(&audit_ctx)
        .await
        .unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("not installed"), "{}", out.message);
    ops.with_file(
        PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
        exec.to_vec(),
    );
    ops.with_file(
        PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
        prep.to_vec(),
    );
    let install_ctx = ctx_with(ops.clone(), RunMode::Install);
    assert_eq!(
        InstallMxc::with_pin(pin.clone())
            .run(&install_ctx)
            .await
            .unwrap()
            .state,
        StepState::Done
    );
    let out = InstallMxc::with_pin(pin).run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
}

#[tokio::test]
async fn install_mxc_unconfigured_fails_closed() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = ctx_with(ops, RunMode::Install);
    let out = InstallMxc::unconfigured().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("no MXC SDK build pinned"));
}

// --- SmokeTest ---
