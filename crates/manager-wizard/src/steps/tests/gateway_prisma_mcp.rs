use super::*;

/// True when an install argv pins prisma to exactly
/// `PRISMA_PINNED_VERSION`: as an explicit `prisma==X` requirement, or
/// (F1 hash-locked install) via `--require-hashes -r <lock>` where the
/// active gateway lock pins that version.
fn pins_prisma_exactly(argv: &[String]) -> bool {
    let pin = format!("prisma=={}", crate::PRISMA_PINNED_VERSION);
    if argv.iter().any(|a| a == &pin) {
        return true;
    }
    argv.iter().any(|a| a == "--require-hashes")
        && argv.iter().any(|a| a == "-r")
        && crate::gateway_lock::parse_requirements_lock(&crate::gateway_lock::active_gateway_lock())
            .map(|reqs| {
                reqs.iter()
                    .any(|r| r.name == "prisma" && r.version == crate::PRISMA_PINNED_VERSION)
            })
            .unwrap_or(false)
}

/// Plant the files that make `postgres_installed` true in the
/// fake fs (binaries in place + the install marker).
pub(super) fn with_postgres_installed(ops: &FakeSystemOps, ctx: &WizardContext) {
    ops.with_file(
        postgres_bin_dir(&ctx.data_dir).join("pg_ctl.exe"),
        Vec::new(),
    );
    ops.with_file(postgres_marker_path(&ctx.data_dir), b"{}".to_vec());
}

/// The schema path the Prisma tests script the resolution probe
/// to report (and plant in the fake fs).
fn prisma_test_schema(ctx: &WizardContext) -> PathBuf {
    ctx.data_dir
        .join("litellm")
        .join("proxy")
        .join("schema.prisma")
}

const PRISMA_SCHEMA_SCRIPT: &str = "import litellm, pathlib; print(pathlib.Path(litellm.__file__).parent / 'proxy' / 'schema.prisma')";

/// The connect probe's argv as the step builds it (the
/// DATABASE_URL lives in the command env, not the argv, so a
/// placeholder URL yields the identical argv to script).
fn prisma_probe_argv(step: &StartGateway, ctx: &WizardContext) -> Vec<String> {
    step.prisma_client_probe_command(ctx, "postgresql://probe")
        .argv
}

fn prisma_not_generated_output() -> CommandOutput {
    CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "RuntimeError: The Client hasn't been generated yet, you must run `prisma generate` before you can use the client.".to_string(),
        }
}

/// (a) PostgreSQL installed, the client was never generated:
/// the probe fails, the step generates, the re-probe passes,
/// and the gateway spawns.
#[tokio::test]
async fn start_gateway_generates_prisma_client_when_missing() {
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
    with_postgres_installed(&ops, &ctx);
    let py = step.venv_python(&ctx).display().to_string();
    let schema = prisma_test_schema(&ctx);
    ops.with_file(schema.clone(), b"// schema".to_vec());
    // Connect probe: fails before generation, passes after.
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command_sequence(
        &probe_refs,
        vec![prisma_not_generated_output(), FakeSystemOps::ok_output("")],
    );
    ops.with_command(
        &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
        FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
    );
    // `prisma generate` itself stays unscripted: default success.
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_cmds();
    // Generation ran exactly once, against the resolved schema.
    let generates: Vec<&PlannedCommand> = executed
        .iter()
        .filter(|c| c.argv.iter().any(|a| a == "generate"))
        .collect();
    assert_eq!(generates.len(), 1, "{executed:?}");
    assert!(generates[0]
        .argv
        .iter()
        .any(|a| a == &schema.display().to_string()));
    // ... with DATABASE_URL set and the venv's script directory
    // first on PATH (the Prisma CLI spawns prisma-client-py by
    // name from there; without it generate fails ENOENT).
    let env = &generates[0].env;
    assert!(env.contains_key("DATABASE_URL"), "{env:?}");
    let path_env = env
        .get("PATH")
        .expect("the generate command carries a PATH");
    assert!(
        path_env.starts_with(&step.venv_scripts_dir(&ctx).display().to_string()),
        "{path_env}"
    );
    // The connect probe ran twice: before and after generation.
    let probes = executed.iter().filter(|c| c.argv == probe_argv).count();
    assert_eq!(probes, 2, "{executed:?}");
    // Each stage is in the step log.
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("Prisma client generated")),
        "{out:?}"
    );
    // And the gateway spawned with DATABASE_URL.
    assert!(ops.spawn_envs()[0].contains_key("DATABASE_URL"));
}

/// (b) Generation fails: the step fails, the generate output is
/// in the message, the manual start block is attached, and
/// nothing spawns.
#[tokio::test]
async fn start_gateway_fails_when_prisma_generate_fails() {
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
    with_postgres_installed(&ops, &ctx);
    let py = step.venv_python(&ctx).display().to_string();
    let schema = prisma_test_schema(&ctx);
    let schema_str = schema.display().to_string();
    ops.with_file(schema.clone(), b"// schema".to_vec());
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command(&probe_refs, prisma_not_generated_output());
    ops.with_command(
        &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
        FakeSystemOps::ok_output(&format!("{schema_str}\n")),
    );
    ops.with_command(
        &[
            py.as_str(),
            "-m",
            "prisma",
            "generate",
            "--schema",
            schema_str.as_str(),
        ],
        CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Error: spawn prisma-client-py ENOENT".to_string(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("prisma generate failed"), "{out:?}");
    assert!(
        out.message.contains("spawn prisma-client-py ENOENT"),
        "{out:?}"
    );
    assert!(out.manual_command.is_some(), "{out:?}");
    assert!(ops.spawns().is_empty(), "no spawn after failed generation");
}

/// (c) No PostgreSQL: the DATABASE_URL path never runs — no
/// probe, no schema resolution, no generate, and the spawn
/// carries no DATABASE_URL.
#[tokio::test]
async fn start_gateway_skips_prisma_generation_without_postgres() {
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
    let executed = ops.executed_commands();
    assert!(
        !executed.iter().any(|argv| argv.iter().any(|a| {
            a.contains("from prisma import Prisma")
                || a == "generate"
                || a.contains("schema.prisma")
        })),
        "{executed:?}"
    );
    assert!(!ops.spawn_envs()[0].contains_key("DATABASE_URL"));
}

/// (d) Client already generated: the probe passes and no
/// generation is issued — the fast path for every later start.
#[tokio::test]
async fn start_gateway_skips_generation_when_client_already_generated() {
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
    with_postgres_installed(&ops, &ctx);
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_commands();
    assert!(
        !executed
            .iter()
            .any(|argv| argv.iter().any(|a| a == "generate")),
        "{executed:?}"
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("Prisma client verified")),
        "{out:?}"
    );
}

/// (f) Generation reports success but the client still fails
/// its standalone connect probe. The step fails with the probe
/// output and the manual block instead of spawning a gateway
/// that cannot use its database.
#[tokio::test]
async fn start_gateway_fails_when_client_still_broken_after_generate() {
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
    with_postgres_installed(&ops, &ctx);
    let py = step.venv_python(&ctx).display().to_string();
    let schema = prisma_test_schema(&ctx);
    ops.with_file(schema.clone(), b"// schema".to_vec());
    ops.with_command(
        &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
        FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
    );
    // Connect probe: not generated, then still failing after
    // generation.
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command_sequence(
        &probe_refs,
        vec![
            prisma_not_generated_output(),
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "prisma engine exited before the connection was established".to_string(),
            },
        ],
    );
    // `prisma generate` itself stays unscripted: default success.
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    let message = out.message.clone();
    assert!(message.contains("engine exited"), "{message}");
    assert!(message.contains("still does not connect"), "{message}");
    assert!(out.manual_command.is_some(), "{out:?}");
    assert!(ops.spawns().is_empty());
}

/// The watchdog disable is set exactly on the Windows +
/// DATABASE_URL combination: the os.kill defect is
/// Windows-only, and the watchdog only runs with a database.
#[test]
fn prisma_watchdog_flag_decision() {
    let url = Some("postgresql://postgres@127.0.0.1:54329/litellm");
    assert!(StartGateway::prisma_watchdog_must_be_disabled(true, url));
    assert!(!StartGateway::prisma_watchdog_must_be_disabled(true, None));
    assert!(!StartGateway::prisma_watchdog_must_be_disabled(false, url));
    assert!(!StartGateway::prisma_watchdog_must_be_disabled(false, None));
}

/// The version probe's argv as the step builds it.
fn prisma_version_argv(step: &StartGateway, ctx: &WizardContext) -> Vec<String> {
    step.prisma_version_command(ctx).argv.clone()
}

/// The LiteLLM version probe's argv as the step builds it.
pub(super) fn litellm_version_argv(step: &StartGateway, ctx: &WizardContext) -> Vec<String> {
    step.litellm_version_command(ctx).argv.clone()
}

#[tokio::test]
async fn start_gateway_records_probed_litellm_version() {
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
    // The probe mirrors the prisma pair: venv python + -c +
    // importlib.metadata for the 'litellm' distribution.
    let version_argv = litellm_version_argv(&step, &ctx);
    assert_eq!(version_argv.len(), 3);
    assert_eq!(version_argv[1], "-c");
    assert!(
        version_argv[2].contains("importlib.metadata")
            && version_argv[2].contains("version('litellm')"),
        "{version_argv:?}"
    );
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("1.105.1\n"));
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("detected LiteLLM version: 1.105.1")),
        "{out:?}"
    );
    // The probed version lands in the recorded spawn state.
    let state = ops
        .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
        .expect("spawn state recorded");
    let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["litellm_version"], "1.105.1");
}

#[tokio::test]
async fn start_gateway_logs_undetermined_litellm_version() {
    // No scripted probe output: the fake's unscripted command
    // succeeds with empty output, which the probe reads as
    // undetermined — the step still succeeds and the log says
    // the version could not be determined (never a guess).
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
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("LiteLLM version could not be determined")),
        "{out:?}"
    );
    let state = ops
        .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
        .expect("spawn state recorded");
    let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["litellm_version"], serde_json::Value::Null);
}

// --- StartGateway: MCP gateway servers (Phase 2) ---

fn mcp_stdio_gateway_server() -> manager_config::McpServerConfig {
    manager_config::McpServerConfig {
        id: "brave_search".to_string(),
        display_name: "Brave Search".to_string(),
        transport: manager_config::McpTransport::Stdio,
        exposure: manager_config::McpExposure::Gateway,
        command: Some("npx".to_string()),
        args: vec![
            "-y".to_string(),
            "@brave/brave-search-mcp-server".to_string(),
        ],
        url: None,
        env_keys: vec!["BRAVE_API_KEY".to_string()],
        cred_ref: Some("local-llm-service-manager/brave-search-mcp".to_string()),
        signup_url: None,
        credential_hint: None,
        allowed_tools: None,
        metered: false,
        rpm_limit: None,
    }
}

fn gateway_ctx_with_mcp(
    ops: Arc<FakeSystemOps>,
    servers: Vec<manager_config::McpServerConfig>,
) -> WizardContext {
    let mut cfg = test_config_with_models();
    cfg.mcp.servers = servers;
    WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops)
}

pub(super) fn script_litellm_version(
    ops: &FakeSystemOps,
    step: &StartGateway,
    ctx: &WizardContext,
    version: &str,
) {
    let argv = litellm_version_argv(step, ctx);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    ops.with_command(&refs, FakeSystemOps::ok_output(&format!("{version}\n")));
}

pub(super) fn fast_step() -> StartGateway {
    StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    }
}

#[tokio::test]
async fn start_gateway_arms_stdio_mcp_server_with_credential() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/mcp/server",
        FakeSystemOps::http_ok(r#"[{"server_id":"brave_search"}]"#),
    );
    ops.with_http_get(
        "http://127.0.0.1:4000/mcp-rest/tools/list?server_id=brave_search",
        FakeSystemOps::http_ok(r#"{"tools":[{"name":"brave_web_search"}]}"#),
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    script_litellm_version(&ops, &step, &ctx, "1.105.1");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // Spawn env: stdio flag + the credential under its env key.
    let envs = ops.spawn_envs();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].get("LITELLM_ENABLE_MCP_STDIO").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        envs[0].get("BRAVE_API_KEY").map(String::as_str),
        Some("brave-secret")
    );
    // YAML registers the server with an env REFERENCE, never the value.
    let cfg = ops
        .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
        .expect("config written");
    let text = String::from_utf8(cfg).unwrap();
    assert!(text.contains("mcp_servers:"), "{text}");
    assert!(text.contains("brave_search"), "{text}");
    assert!(
        text.contains("require_key_mcp_access_defined: true"),
        "{text}"
    );
    assert!(!text.contains("brave-secret"), "{text}");
    // Load check passed and is logged.
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("MCP load check passed for 'brave_search'")),
        "{out:?}"
    );
    // Fingerprint records the armed set.
    let state = ops
        .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
        .expect("spawn state recorded");
    let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["fingerprint"]["mcp_stdio_enabled"], true);
    assert!(
        !state["fingerprint"]["mcp_servers_hash"]
            .as_str()
            .unwrap()
            .is_empty(),
        "{state}"
    );
}

#[tokio::test]
async fn start_gateway_unarmed_mcp_server_is_omitted_and_gateway_still_starts() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    // No credential scripted: the arming read fails.
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    // No LiteLLM version scripted either: with no armed stdio
    // server the floor check must not fire.
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("'brave_search' is not armed")),
        "{out:?}"
    );
    let envs = ops.spawn_envs();
    assert_eq!(envs.len(), 1);
    assert!(!envs[0].contains_key("LITELLM_ENABLE_MCP_STDIO"));
    assert!(!envs[0].contains_key("BRAVE_API_KEY"));
    let cfg = ops
        .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
        .expect("config written");
    let text = String::from_utf8(cfg).unwrap();
    assert!(!text.contains("mcp_servers"), "{text}");
    assert!(!text.contains("brave_search"), "{text}");
    let state = ops
        .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
        .expect("spawn state recorded");
    let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["fingerprint"]["mcp_stdio_enabled"], false);
    assert_eq!(state["fingerprint"]["mcp_servers_hash"], "");
}

#[tokio::test]
async fn start_gateway_fails_closed_below_mcp_floor_with_armed_stdio() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    script_litellm_version(&ops, &step, &ctx, "1.80.0");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("1.83.7"), "{}", out.message);
    assert!(out.message.contains("pinned_version"), "{}", out.message);
    assert!(ops.spawns().is_empty(), "floor failure precedes any spawn");
}

#[tokio::test]
async fn start_gateway_fails_closed_when_version_undetermined_with_armed_stdio() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    // Version probe unscripted → undetermined → fail-closed.
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("unknown"), "{}", out.message);
    assert!(ops.spawns().is_empty());
}

#[tokio::test]
async fn start_gateway_mcp_load_check_failure_is_noted_not_fatal() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    // The gateway registered nothing, and the tools list errors.
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/mcp/server",
        FakeSystemOps::http_ok("[]"),
    );
    ops.with_http_get(
        "http://127.0.0.1:4000/mcp-rest/tools/list?server_id=brave_search",
        crate::HttpResponse {
            status: 500,
            body: br#"{"error":{"message":"server failed to start"}}"#.to_vec(),
            elapsed_ms: 1,
        },
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    script_litellm_version(&ops, &step, &ctx, "1.105.1");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message
            .contains("MCP load check failed for: brave_search"),
        "{out:?}"
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("server failed to start")),
        "{out:?}"
    );
}

#[tokio::test]
async fn start_gateway_mcp_load_check_warns_on_unmanaged_registered_servers() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/mcp/server",
        FakeSystemOps::http_ok(r#"[{"server_id":"brave_search"},{"server_id":"db_extra"}]"#),
    );
    ops.with_http_get(
        "http://127.0.0.1:4000/mcp-rest/tools/list?server_id=brave_search",
        FakeSystemOps::http_ok(r#"{"tools":[{"name":"brave_web_search"}]}"#),
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    script_litellm_version(&ops, &step, &ctx, "1.105.1");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("'db_extra'") && l.contains("not in the generated configuration")),
        "{out:?}"
    );
    assert!(!out.message.contains("MCP load check failed"), "{out:?}");
}

#[test]
fn start_gateway_planned_spawn_carries_mcp_env_placeholders() {
    let ops = Arc::new(FakeSystemOps::new());
    let step = fast_step();
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    let planned = step.planned_commands(&ctx);
    let plan_spawn = planned
        .iter()
        .find(|c| c.argv.iter().any(|a| a == "--config"))
        .expect("gateway spawn planned");
    assert_eq!(
        plan_spawn
            .env
            .get("LITELLM_ENABLE_MCP_STDIO")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        plan_spawn.env.get("BRAVE_API_KEY").map(String::as_str),
        Some("<set from credential manager>")
    );
}

fn gateway_ctx_with_searxng(ops: Arc<FakeSystemOps>, api_base: &str) -> WizardContext {
    let mut cfg = test_config_with_models();
    cfg.mcp.servers = vec![];
    cfg.mcp.searxng = manager_config::SearxngSettings {
        enabled: true,
        api_base: api_base.to_string(),
    };
    WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops)
}

#[tokio::test]
async fn start_gateway_searxng_env_yaml_and_fingerprint() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = fast_step();
    let ctx = gateway_ctx_with_searxng(ops.clone(), "http://127.0.0.1:8080");
    script_litellm_version(&ops, &step, &ctx, "1.105.1");
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // Spawn env carries the real base (config, not a secret).
    let envs = ops.spawn_envs();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].get("SEARXNG_API_BASE").map(String::as_str),
        Some("http://127.0.0.1:8080")
    );
    // YAML gains the top-level search_tools block.
    let cfg = ops
        .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
        .expect("config written");
    let text = String::from_utf8(cfg).unwrap();
    assert!(text.contains("search_tools:"), "{text}");
    assert!(text.contains("search_provider: searxng"), "{text}");
    // Fingerprint records the SearXNG contribution.
    let state = ops
        .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
        .expect("spawn state recorded");
    let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(
        state["fingerprint"]["searxng_hash"].as_str().unwrap().len(),
        64,
        "{state}"
    );
}

#[test]
fn start_gateway_planned_spawn_carries_searxng_real_value() {
    let ops = Arc::new(FakeSystemOps::new());
    let step = fast_step();
    let ctx = gateway_ctx_with_searxng(ops.clone(), "http://127.0.0.1:8080");
    let planned = step.planned_commands(&ctx);
    let plan_spawn = planned
        .iter()
        .find(|c| c.argv.iter().any(|a| a == "--config"))
        .expect("gateway spawn planned");
    // The real value, not a placeholder: api_base is config.
    assert_eq!(
        plan_spawn.env.get("SEARXNG_API_BASE").map(String::as_str),
        Some("http://127.0.0.1:8080")
    );
    // Disabled: no entry in the plan at all.
    let mut cfg = test_config_with_models();
    cfg.mcp.servers = vec![];
    let ctx = WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops);
    let planned = step.planned_commands(&ctx);
    let plan_spawn = planned
        .iter()
        .find(|c| c.argv.iter().any(|a| a == "--config"))
        .expect("gateway spawn planned");
    assert!(!plan_spawn.env.contains_key("SEARXNG_API_BASE"));
}

#[tokio::test]
async fn manual_start_block_carries_searxng_base() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = gateway_ctx_with_searxng(ops.clone(), "http://127.0.0.1:8080");
    let block = StartGateway::default().manual_start_block(&ctx).await;
    assert!(
        block.contains("$env:SEARXNG_API_BASE = \"http://127.0.0.1:8080\""),
        "{block}"
    );
}

#[tokio::test]
async fn manual_start_block_carries_mcp_stdio_flag_and_credential_notes() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
    let block = StartGateway::default().manual_start_block(&ctx).await;
    assert!(
        block.contains("$env:LITELLM_ENABLE_MCP_STDIO = \"true\""),
        "{block}"
    );
    assert!(block.contains("$env:BRAVE_API_KEY"), "{block}");
    assert!(
        block.contains("local-llm-service-manager/brave-search-mcp"),
        "{block}"
    );
}

/// (g) The installed prisma already matches the pin: no
/// directory removal, no reinstall, no generation — the probe
/// fast path runs as before.
#[tokio::test]
async fn start_gateway_keeps_matching_prisma_pin_untouched() {
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
    with_postgres_installed(&ops, &ctx);
    let version_argv = prisma_version_argv(&step, &ctx);
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops.removed_dirs().is_empty(), "{:?}", ops.removed_dirs());
    let executed = ops.executed_commands();
    assert!(
        !executed
            .iter()
            .any(|argv| argv.iter().any(|a| a == "--reinstall-package")),
        "no pin reinstall when the version matches: {executed:?}"
    );
    assert!(
        !executed
            .iter()
            .any(|argv| argv.iter().any(|a| a == "generate")),
        "{executed:?}"
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("Prisma client verified")),
        "{out:?}"
    );
}

/// (g2) Verify-first fast path: pinned version + generated
/// client present + connect probe passing means the start
/// mutates NOTHING — no reinstall, no generation, no repair
/// state — and records the spawn state for the drift check.
/// This is the previously-running-state start: seconds, not
/// minutes.
#[tokio::test]
async fn start_gateway_verifies_healthy_prisma_without_mutating() {
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
    with_postgres_installed(&ops, &ctx);
    // The generated client's marker file is in place.
    ops.with_file(step.generated_client_path(&ctx), b"# generated".to_vec());
    let version_argv = prisma_version_argv(&step, &ctx);
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail.iter().any(|l| l.contains("no changes made")),
        "{out:?}"
    );
    let executed = ops.executed_commands();
    assert!(
        !executed.iter().any(|argv| argv
            .iter()
            .any(|a| a == "--reinstall-package" || a == "generate")),
        "verification performed mutation: {executed:?}"
    );
    assert!(ops.removed_dirs().is_empty(), "{:?}", ops.removed_dirs());
    // No repair state was written (or left) on the fast path.
    assert!(ops
        .file_bytes(&StartGateway::prisma_repair_state_path(&ctx))
        .is_none());
    // The spawn state WAS recorded for the next drift check.
    let state = ops
        .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
        .expect("spawn state recorded");
    let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["fingerprint"]["port"], 4000);
    assert_eq!(state["fingerprint"]["postgres_port"], 54329);
    assert!(
        !state["fingerprint"]["master_key_hash"]
            .as_str()
            .unwrap()
            .is_empty(),
        "the spawned master key is fingerprinted: {state}"
    );
}

/// (g3) An interrupted repair (repair-state file present) is
/// resumed and completed: the log says so, generation runs,
/// and the state file is gone at the end.
#[tokio::test]
async fn start_gateway_resumes_interrupted_prisma_repair() {
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
    with_postgres_installed(&ops, &ctx);
    // The leftover of an interrupted repair.
    ops.with_file(
        StartGateway::prisma_repair_state_path(&ctx),
        br#"{"pin":"0.11.0","phase":"pin"}"#.to_vec(),
    );
    let py = step.venv_python(&ctx).display().to_string();
    let schema = prisma_test_schema(&ctx);
    ops.with_file(schema.clone(), b"// schema".to_vec());
    let version_argv = prisma_version_argv(&step, &ctx);
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
    ops.with_command(
        &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
        FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
    );
    // The client was never generated (the interruption): the
    // probe fails, generation runs, the re-probe passes.
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command_sequence(
        &probe_refs,
        vec![prisma_not_generated_output(), FakeSystemOps::ok_output("")],
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail.iter().any(|l| l.contains("did not complete")),
        "the resume is logged: {out:?}"
    );
    let executed = ops.executed_commands();
    assert_eq!(
        executed
            .iter()
            .filter(|argv| argv.iter().any(|a| a == "generate"))
            .count(),
        1,
        "{executed:?}"
    );
    assert!(
        ops.file_bytes(&StartGateway::prisma_repair_state_path(&ctx))
            .is_none(),
        "a completed repair clears its state"
    );
}

/// (h) The installed prisma is 0.15.0 — the state that passes a
/// standalone connect but breaks LiteLLM's wrapper. The step
/// must not trust the probe: it clears the stale generated
/// client (the package directory), reinstalls the pinned
/// package, and only then probes and generates.
#[tokio::test]
async fn start_gateway_reinstalls_pinned_prisma_and_clears_stale_client_on_mismatch() {
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
    with_postgres_installed(&ops, &ctx);
    let py = step.venv_python(&ctx).display().to_string();
    let schema = prisma_test_schema(&ctx);
    ops.with_file(schema.clone(), b"// schema".to_vec());
    // The stale 0.15 package directory is present in the venv.
    ops.with_file(step.prisma_package_dir(&ctx), Vec::new());
    let version_argv = prisma_version_argv(&step, &ctx);
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("0.15.0\n"));
    ops.with_command(
        &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
        FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
    );
    // After the clear + reinstall the package is back to its
    // ungenerated wheel state: the first probe fails as
    // not-generated, generation runs, the re-probe passes.
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command_sequence(
        &probe_refs,
        vec![prisma_not_generated_output(), FakeSystemOps::ok_output("")],
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // The stale package directory was removed exactly once.
    assert_eq!(ops.removed_dirs(), vec![step.prisma_package_dir(&ctx)]);
    let executed = ops.executed_cmds();
    // The pin reinstall ran (exact pin, targeted reinstall)...
    let pin_at = executed
        .iter()
        .position(|c| c.argv.iter().any(|a| a == "--reinstall-package"))
        .expect("the pinned prisma reinstall ran");
    assert!(pins_prisma_exactly(&executed[pin_at].argv), "{executed:?}");
    // ... before the first connect probe.
    let probe_at = executed
        .iter()
        .position(|c| c.argv == probe_argv)
        .expect("the connect probe ran");
    assert!(pin_at < probe_at, "{executed:?}");
    // Generation ran once, after the reinstall.
    let generates = executed
        .iter()
        .filter(|c| c.argv.iter().any(|a| a == "generate"))
        .count();
    assert_eq!(generates, 1, "{executed:?}");
    // The mismatch was logged.
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("does not match the pinned")),
        "{out:?}"
    );
    assert!(ops.spawn_envs()[0].contains_key("DATABASE_URL"));
}

/// (i) Sustained health, database path: the gateway answers
/// its first probe, then stops answering inside the
/// confirmation window — the watchdog-kill shape. The step
/// fails instead of reporting a gateway that is already gone.
#[tokio::test]
async fn start_gateway_db_path_fails_when_gateway_dies_inside_confirmation_window() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get_sequence(
        "http://127.0.0.1:4000/v1/models",
        vec![
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
            crate::HttpResponse {
                status: 503,
                body: Vec::new(),
                elapsed_ms: 1,
            },
        ],
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 50,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_postgres_installed(&ops, &ctx);
    let version_argv = prisma_version_argv(&step, &ctx);
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("did not stay up"), "{out:?}");
    assert!(out.manual_command.is_some(), "{out:?}");
    assert_eq!(ops.spawns().len(), 1);
}

/// (j) Sustained health, database path, gateway stays up: the
/// step succeeds and the confirmation window actually probed
/// (more than the single first-healthy GET).
#[tokio::test]
async fn start_gateway_db_path_sustained_health_passes() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 25,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    with_postgres_installed(&ops, &ctx);
    let version_argv = prisma_version_argv(&step, &ctx);
    let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
    ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
    let probe_argv = prisma_probe_argv(&step, &ctx);
    let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
    ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        ops.gets().len() > 1,
        "the confirmation window kept probing: {:?}",
        ops.gets()
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("confirmation window")),
        "{out:?}"
    );
}

/// (k) Without a database there is no confirmation window,
/// however long it is configured: the only GETs are the first
/// healthy probe and the step's single final re-check — the
/// configured 5 s window would have probed many more times.
#[tokio::test]
async fn start_gateway_without_database_skips_confirmation_window() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 5_000,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.gets().len(), 2, "{:?}", ops.gets());
}

/// (l) Spawn env on the database path: DATABASE_URL is set,
/// the fresh-venv pip install carries the exact prisma pin,
/// and the watchdog flag is present exactly when this build
/// targets Windows (the defect it disables is Windows-only).
/// Without a database, neither variable appears.
#[tokio::test]
async fn start_gateway_spawn_env_database_path() {
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
    with_postgres_installed(&ops, &ctx);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let env = &ops.spawn_envs()[0];
    assert!(env.contains_key("DATABASE_URL"), "{env:?}");
    assert_eq!(
        env.contains_key("PRISMA_HEALTH_WATCHDOG_ENABLED"),
        cfg!(target_os = "windows"),
        "{env:?}"
    );
    if let Some(v) = env.get("PRISMA_HEALTH_WATCHDOG_ENABLED") {
        assert_eq!(v, "false");
    }
    // The install that built this venv pinned prisma exactly — either
    // as an explicit requirement or through the hash lock (F1).
    let executed = ops.executed_commands();
    assert!(
        executed.iter().any(|argv| pins_prisma_exactly(argv)),
        "{executed:?}"
    );
    // Database-free: neither variable.
    let ops2 = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops2);
    ops2.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
    let out2 = step.run(&ctx2).await.unwrap();
    assert_eq!(out2.state, StepState::Done, "{out2:?}");
    let env2 = &ops2.spawn_envs()[0];
    assert!(!env2.contains_key("DATABASE_URL"), "{env2:?}");
    assert!(
        !env2.contains_key("PRISMA_HEALTH_WATCHDOG_ENABLED"),
        "{env2:?}"
    );
}

/// (e) Health-check failure: the failure MESSAGE is the step's
/// own verdict; the spawned gateway's stderr tail rides in the
/// step LOG, labeled as gateway output. (It previously sat in
/// the message itself, presenting gateway log noise as the
/// step's error.)
#[tokio::test]
async fn start_gateway_health_failure_includes_gateway_stderr_tail() {
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_dead_spawn();
    let step = StartGateway {
        health_retries: 2,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    // The spawned gateway's captured stderr, planted where the
    // spawn redirects it.
    ops.with_file(
            step.gateway_stderr_log(&ctx),
            b"20:12:30 - LiteLLM Proxy:ERROR: utils.py:4432 - Failed to import Prisma client: The Client hasn't been generated yet\nRuntimeError: The Client hasn't been generated yet, you must run `prisma generate` before you can use the client.\n"
                .to_vec(),
        );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("did not become healthy"), "{out:?}");
    assert!(
        !out.message.contains("hasn't been generated"),
        "gateway output leaked into the step's verdict: {out:?}"
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("gateway stderr") && l.contains("last lines")),
        "log carries the labeled gateway tail: {out:?}"
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("The Client hasn't been generated yet")),
        "log carries the gateway's own diagnosis: {out:?}"
    );
}

#[tokio::test]
async fn start_gateway_audit_probes_health() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok("{}"),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = StartGateway::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);
    assert!(ops.spawns().is_empty(), "audit spawns nothing");
}

#[test]
fn find_uv_in_detects_binary() {
    let dir = path_dir_with_uv(true);
    let found = StartGateway::find_uv_in(std::slice::from_ref(&dir));
    assert!(found.is_some());
    let empty = path_dir_with_uv(false);
    assert!(StartGateway::find_uv_in(std::slice::from_ref(&empty)).is_none());
    assert!(StartGateway::find_uv_in(&[]).is_none());
}

#[tokio::test]
async fn start_gateway_uses_bundled_uv_when_present() {
    // uv not on PATH, but the bootstrapped copy exists in the fake fs.
    let _guard = PathGuard::set_to(&path_dir_with_uv(false));
    let ops = Arc::new(FakeSystemOps::new());
    let name = if cfg!(target_os = "windows") {
        "uv.exe"
    } else {
        "uv"
    };
    ops.with_file(PathBuf::from("/data/bin").join(name), b"fake-uv".to_vec());
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
    let executed = ops.executed_commands();
    assert!(executed[0][0].ends_with(name), "{:?}", executed[0]);
    // No bootstrap download happened.
    assert!(ops.downloaded_to().is_empty());
}

#[tokio::test]
async fn start_gateway_planned_shows_bootstrap_when_uv_missing() {
    let _guard = PathGuard::set_to(&path_dir_with_uv(false));
    let ctx = ctx_with(Arc::new(FakeSystemOps::new()), RunMode::Install);
    let planned = StartGateway::default().planned_commands(&ctx);
    if cfg!(target_os = "windows") {
        // Bootstrap download + the seven commands of the
        // uv-present plan (two probes, venv, pip, spawn, two
        // firewall commands).
        assert_eq!(planned.len(), 8);
        assert_eq!(planned[0].argv[0], "download");
        assert!(
            planned[0].argv[1].contains("astral-sh/uv"),
            "{:?}",
            planned[0].argv
        );
    } else {
        // No auto-bootstrap off Windows: uv path shown, run() fails
        // with install guidance instead.
        assert_eq!(planned.len(), 7);
    }
}

#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn start_gateway_fails_with_install_guidance_without_uv() {
    let _guard = PathGuard::set_to(&path_dir_with_uv(false));
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = ctx_with(ops, RunMode::Install);
    let out = StartGateway::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(
        out.message
            .contains("auto-bootstrap is only supported on Windows"),
        "{out:?}"
    );
}

// --- InstallLlamaCpp: VC++ redistributable ---
