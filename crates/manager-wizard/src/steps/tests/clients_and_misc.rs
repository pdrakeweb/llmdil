use super::*;

#[cfg(not(target_os = "windows"))]
#[test]
fn vcredist_check_passes_off_windows() {
    // The probe is Windows-only; other platforms always pass.
    assert!(InstallLlamaCpp::vcredist_present(&PathBuf::from(
        "/nonexistent-system32"
    )));
}

#[test]
fn llamacpp_offers_vcredist_download_url() {
    assert_eq!(
        InstallLlamaCpp::with_pin(test_pin()).help_url(),
        Some(crate::VC_REDIST_DOWNLOAD_URL)
    );
    assert!(crate::VC_REDIST_DOWNLOAD_URL.starts_with("https://"));
}

// --- DownloadModels: disk-space pre-check ---

#[tokio::test]
async fn download_models_fails_when_disk_full() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_head_length("https://example.com/qwen3-8b.gguf", 50_000_000_000);
    ops.with_head_length("https://example.com/qwen2.5-coder-7b.gguf", 40_000_000_000);
    ops.with_free_space(1_000_000);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("insufficient disk space"), "{out:?}");
}

#[tokio::test]
async fn download_models_proceeds_when_space_sufficient() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_head_length("https://example.com/qwen3-8b.gguf", 5_000_000_000);
    ops.with_head_length("https://example.com/qwen2.5-coder-7b.gguf", 4_000_000_000);
    ops.with_free_space(100_000_000_000);
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"gguf-7b-bytes".to_vec(),
    );
    let ctx = ctx_with(ops, RunMode::Install);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
}

// --- VscodeExtensions ---

#[tokio::test]
async fn vscode_install_writes_config_without_clobbering() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_home(PathBuf::from("/home/test"));
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = VscodeExtensions::default();
    assert_eq!(step.planned_commands(&ctx).len(), 3);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message.contains("Continue configured"),
        "{}",
        out.message
    );
    // The `code --version` probe plus three extension installs ran.
    assert_eq!(ops.executed_commands().len(), 4);
    // Canonical config.yaml + .env written (nothing existed).
    let canonical = ops
        .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
        .expect("canonical config.yaml written");
    let text = String::from_utf8(canonical).unwrap();
    assert!(text.contains("http://127.0.0.1:4000/v1"), "{text}");
    assert!(text.contains("planner"), "{text}");
    assert!(text.contains("${{ secrets.LITELLM_API_KEY }}"), "{text}");
    let env = ops
        .file_bytes(&PathBuf::from("/home/test/.continue/.env"))
        .expect(".env written");
    let env_text = String::from_utf8(env).unwrap();
    assert!(env_text.contains("LITELLM_API_KEY=sk-"), "{env_text}");
    // No legacy JSON is created.
    assert!(ops
        .file_bytes(&PathBuf::from("/home/test/.continue/config.json"))
        .is_none());

    // Second run: the YAML is merged in place — still exactly
    // one entry per gateway group, no sidecar.
    let out2 = step.run(&ctx).await.unwrap();
    assert_eq!(out2.state, StepState::Done);
    let canonical = ops
        .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
        .unwrap();
    let v: serde_yaml::Value =
        serde_yaml::from_str(&String::from_utf8(canonical).unwrap()).unwrap();
    assert_eq!(v["models"].as_sequence().unwrap().len(), 4);
    assert!(ops
        .file_bytes(&PathBuf::from(
            "/home/test/.continue/config.yaml.llm-manager"
        ))
        .is_none());
}

#[tokio::test]
async fn vscode_install_merges_existing_continue_yaml_and_env() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_home(PathBuf::from("/home/test"));
    // Pete's own config.yaml: another model and top-level content.
    ops.with_file(
            PathBuf::from("/home/test/.continue/config.yaml"),
            b"name: My Config\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: cloud (work)\n    provider: openai\n    model: gpt-5\n    apiBase: https://api.openai.com/v1\n    apiKey: literal\n".to_vec(),
        );
    ops.with_file(
        PathBuf::from("/home/test/.continue/.env"),
        b"# my secrets\nOTHER_KEY=keep-me\nLITELLM_API_KEY=sk-stale\n".to_vec(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = VscodeExtensions::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let canonical = ops
        .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
        .unwrap();
    let v: serde_yaml::Value =
        serde_yaml::from_str(&String::from_utf8(canonical).unwrap()).unwrap();
    assert_eq!(v["name"].as_str().unwrap(), "My Config");
    let models = v["models"].as_sequence().unwrap();
    assert_eq!(models.len(), 5, "his model plus the four gateway models");
    assert!(models.iter().any(|m| m["model"].as_str() == Some("gpt-5")));
    assert!(models
        .iter()
        .any(|m| m["model"].as_str() == Some("planner")));
    // .env: our key rotated, his other variable preserved.
    let env = ops
        .file_bytes(&PathBuf::from("/home/test/.continue/.env"))
        .unwrap();
    let env_text = String::from_utf8(env).unwrap();
    assert!(env_text.contains("OTHER_KEY=keep-me"), "{env_text}");
    assert!(!env_text.contains("sk-stale"), "{env_text}");
    assert_eq!(env_text.matches("LITELLM_API_KEY=").count(), 1);
}

#[tokio::test]
async fn vscode_install_retires_wizard_written_legacy_json() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_home(PathBuf::from("/home/test"));
    // A config.json in exactly the shape older wizard versions wrote.
    let legacy = serde_json::json!({"models": [{
        "title": "planner (local)", "provider": "openai", "model": "planner",
        "apiBase": "http://127.0.0.1:4000/v1", "apiKey": "sk-old"
    }]});
    ops.with_file(
        PathBuf::from("/home/test/.continue/config.json"),
        serde_json::to_vec(&legacy).unwrap(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = VscodeExtensions::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops
        .file_bytes(&PathBuf::from("/home/test/.continue/config.json"))
        .is_none());
    assert!(ops
        .file_bytes(&PathBuf::from(
            "/home/test/.continue/config.json.llm-manager.bak"
        ))
        .is_some());
    assert!(ops
        .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
        .is_some());
}

#[test]
fn env_merge_replaces_and_appends_by_key() {
    let merged = merge_env_contents("A=1\nLITELLM_API_KEY=old\nB=2\n", "LITELLM_API_KEY", "new");
    assert_eq!(merged, "A=1\nLITELLM_API_KEY=new\nB=2\n");
    let appended = merge_env_contents("A=1\n", "LITELLM_API_KEY", "new");
    assert_eq!(appended, "A=1\nLITELLM_API_KEY=new\n");
    let exported = merge_env_contents("export LITELLM_API_KEY=old\n", "LITELLM_API_KEY", "new");
    assert_eq!(exported, "LITELLM_API_KEY=new\n");
}

#[tokio::test]
async fn vscode_install_uses_resolved_cli() {
    // The app process's PATH cannot see VS Code, but the ops
    // layer resolved the install (registry / install locations):
    // every command must use the resolved absolute path.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_home(PathBuf::from("/home/test"));
    ops.with_vscode_cli("C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd");
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = VscodeExtensions::default();
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let executed = ops.executed_commands();
    assert_eq!(executed.len(), 4);
    assert!(
        executed
            .iter()
            .all(|a| a[0] == "C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd"),
        "{executed:?}"
    );
}

#[tokio::test]
async fn vscode_install_skips_when_code_cli_missing() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command_failure(
        &["code", "--version"],
        -1,
        "spawn failed: program not found",
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = VscodeExtensions::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Skipped, "{out:?}");
    assert!(out.message.contains("not installed"), "{}", out.message);
    // Only the probe ran — no install was attempted.
    assert_eq!(ops.executed_commands().len(), 1);
}

#[tokio::test]
async fn vscode_audit_skips_when_code_cli_missing() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command_failure(
        &["code", "--list-extensions"],
        -1,
        "spawn failed: program not found",
    );
    let ctx = ctx_with(ops, RunMode::Audit);
    let out = VscodeExtensions::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Skipped, "{out:?}");
}

#[tokio::test]
async fn vscode_audit_checks_presence() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_listed_extensions(&[
        "continue.continue",
        "saoudrizwan.claude-dev",
        "vivswan.litellm-vscode-chat",
        "other",
    ]);
    let ctx = ctx_with(ops, RunMode::Audit);
    let out = VscodeExtensions::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);

    let ops2 = Arc::new(FakeSystemOps::new());
    ops2.with_listed_extensions(&["continue.continue"]);
    let ctx2 = ctx_with(ops2, RunMode::Audit);
    let out2 = VscodeExtensions::default().run(&ctx2).await.unwrap();
    assert_eq!(out2.state, StepState::Failed);
    assert!(out2.message.contains("saoudrizwan.claude-dev"));
}

#[test]
fn litellm_settings_merge_preserves_user_content_and_replaces_own_entry() {
    let existing = "{\n    // Pete's settings\n    \"editor.fontSize\": 14,\n    \"litellm-vscode-chat.servers\": [\n        { \"label\": \"Work\", \"baseUrl\": \"http://example:4000\" },\n        { \"label\": \"Local\", \"baseUrl\": \"http://127.0.0.1:4000\", \"auth\": { \"apiKey\": \"sk-stale\" } }\n    ]\n}\n";
    let entry = litellm_server_entry(4000, "sk-fresh");
    let merged = merge_litellm_server_setting(existing, &entry).unwrap();
    // The user's comment and unrelated setting survive.
    assert!(merged.contains("// Pete's settings"), "{merged}");
    assert!(merged.contains("\"editor.fontSize\": 14"), "{merged}");
    // Exactly one Local entry, carrying the fresh key; the other
    // server is untouched; no stale key remains.
    let v = jsonc_parser::parse_to_serde_value(&merged, &Default::default())
        .unwrap()
        .unwrap();
    let servers = v["litellm-vscode-chat.servers"].as_array().unwrap();
    assert_eq!(servers.len(), 2);
    let locals: Vec<_> = servers.iter().filter(|s| s["label"] == "Local").collect();
    assert_eq!(locals.len(), 1);
    assert_eq!(locals[0]["auth"]["apiKey"], "sk-fresh");
    assert!(servers.iter().any(|s| s["label"] == "Work"));
    assert!(!merged.contains("sk-stale"), "{merged}");
    // Merging again is a no-op (no duplicate append).
    let merged2 = merge_litellm_server_setting(&merged, &entry).unwrap();
    let v2 = jsonc_parser::parse_to_serde_value(&merged2, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(
        v2["litellm-vscode-chat.servers"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn litellm_settings_merge_into_empty_settings() {
    let merged = merge_litellm_server_setting("{}", &litellm_server_entry(4000, "sk-k")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
    assert_eq!(v["litellm-vscode-chat.servers"][0]["label"], "Local");
    assert_eq!(
        v["litellm-vscode-chat.servers"][0]["baseUrl"],
        "http://127.0.0.1:4000"
    );
}

#[tokio::test]
async fn vscode_install_configures_litellm_endpoint_and_agents() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_home(PathBuf::from("/home/test"));
    ops.with_credential_value(crate::LITELLM_MASTER_KEY_CRED_TARGET, "sk-test-master");
    // Pre-existing settings with an unrelated key.
    ops.with_file(
        PathBuf::from("/home/test/AppData/Roaming/Code/User/settings.json"),
        b"{\n    \"editor.fontSize\": 13\n}\n".to_vec(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = VscodeExtensions::default().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message.contains("LiteLLM endpoint configured"),
        "{}",
        out.message
    );
    assert!(
        out.message.contains("Copilot agent(s) written"),
        "{}",
        out.message
    );
    // The extension itself was installed.
    assert!(ops
        .executed_commands()
        .iter()
        .any(|a| a.iter().any(|x| x == "vivswan.litellm-vscode-chat")));
    // Settings: entry present with the master key, unrelated key kept.
    let settings = ops
        .file_bytes(&PathBuf::from(
            "/home/test/AppData/Roaming/Code/User/settings.json",
        ))
        .expect("settings written");
    let text = String::from_utf8(settings).unwrap();
    assert!(text.contains("\"editor.fontSize\": 13"), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let servers = v["litellm-vscode-chat.servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["baseUrl"], "http://127.0.0.1:4000");
    assert_eq!(servers[0]["auth"]["apiKey"], "sk-test-master");
    // Agent files: planner carries Pete's text; coder carries
    // edit/execute; every configured group got a file.
    let planner = ops
        .file_bytes(&PathBuf::from(
            "/home/test/.copilot/agents/litellm-planner.md",
        ))
        .expect("planner agent written");
    let planner_text = String::from_utf8(planner).unwrap();
    assert!(
        planner_text.contains("name: LiteLLM Planner"),
        "{planner_text}"
    );
    assert!(planner_text.contains("model: \"litellm/planner\""));
    assert!(planner_text.contains("tools: [read, search]"));
    assert!(planner_text.contains("software planning agent"));
    let coder = ops
        .file_bytes(&PathBuf::from(
            "/home/test/.copilot/agents/litellm-coder.md",
        ))
        .expect("coder agent written");
    let coder_text = String::from_utf8(coder).unwrap();
    assert!(coder_text.contains("tools: [read, search, edit, execute]"));
    for group in ["coder-fast", "tool-runner"] {
        assert!(
            ops.file_bytes(&PathBuf::from(format!(
                "/home/test/.copilot/agents/litellm-{group}.md"
            )))
            .is_some(),
            "agent for {group}"
        );
    }
}

#[tokio::test]
async fn vscode_audit_verifies_endpoint_and_agents_after_install() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_home(PathBuf::from("/home/test"));
    ops.with_listed_extensions(&[
        "continue.continue",
        "saoudrizwan.claude-dev",
        "vivswan.litellm-vscode-chat",
    ]);
    let install_ctx = ctx_with(ops.clone(), RunMode::Install);
    assert_eq!(
        VscodeExtensions::default()
            .run(&install_ctx)
            .await
            .unwrap()
            .state,
        StepState::Done
    );
    let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = VscodeExtensions::default().run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // Remove one agent file: the audit must now fail and name it.
    crate::SystemOps::remove_file(
        ops.as_ref(),
        &PathBuf::from("/home/test/.copilot/agents/litellm-coder.md"),
    )
    .await
    .unwrap();
    let out = VscodeExtensions::default().run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("coder"), "{}", out.message);
}

#[tokio::test]
async fn smoke_test_fails_when_backend_never_healthy() {
    let ops = Arc::new(FakeSystemOps::new());
    script_backends_healthy(&ops);
    // The planner backend (first group member polled) stays in
    // its model-loading state.
    ops.with_http_get(
        "http://127.0.0.1:8081/health",
        crate::HttpResponse {
            status: 503,
            body: b"loading".to_vec(),
            elapsed_ms: 1,
        },
    );
    ops.with_http_post(
        "http://127.0.0.1:4000/v1/chat/completions",
        smoke_post_response(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = SmokeTest {
        max_latency_ms: SMOKE_MAX_LATENCY_MS,
        ready_budget_ms: 40,
        ready_poll_ms: 5,
    };
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(
        out.message.contains("did not become healthy"),
        "{}",
        out.message
    );
    assert!(out.message.contains("planner"), "{}", out.message);
    // No gateway probe was attempted.
    assert!(ops.posts().is_empty());
}

// --- McpServers ---

/// A context whose MCP registry is exactly `servers` (the
/// default test config seeds the full registry, curated
/// servers included; the Phase 4 step checks them all).
fn ctx_with_mcp_servers(
    ops: Arc<FakeSystemOps>,
    servers: Vec<manager_config::McpServerConfig>,
) -> WizardContext {
    let mut config = test_config_with_models();
    config.mcp.servers = servers;
    WizardContext::new(config, PathBuf::from("/data"), RunMode::Install, ops)
}

fn mcp_test_server(
    id: &str,
    exposure: McpExposure,
    cred_ref: Option<&str>,
) -> manager_config::McpServerConfig {
    manager_config::McpServerConfig {
        id: id.to_string(),
        display_name: id.to_string(),
        transport: manager_config::McpTransport::Stdio,
        exposure,
        command: Some("npx".to_string()),
        args: vec!["-y".to_string(), format!("@example/{id}@1.0.0")],
        url: None,
        env_keys: vec!["EXAMPLE_API_KEY".to_string()],
        cred_ref: cred_ref.map(str::to_string),
        signup_url: None,
        credential_hint: None,
        allowed_tools: None,
        metered: false,
        rpm_limit: None,
    }
}

#[tokio::test]
async fn mcp_servers_requires_credentials() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = ctx_with(ops, RunMode::Install);
    let out = McpServers.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    // Target name shown; values never shown (there are none to show).
    assert!(out.message.contains(GITHUB_CRED_TARGET), "{}", out.message);

    // Phase 4: the check is registry-wide, so the pass case
    // needs the curated brave_search credential too (the
    // default test config's registry seeds it Gateway-exposed).
    let ops2 = Arc::new(FakeSystemOps::new());
    ops2.with_credential(GITHUB_CRED_TARGET);
    ops2.with_credential(GDRIVE_CRED_TARGET);
    ops2.with_credential(manager_config::BRAVE_SEARCH_MCP_CRED_REF);
    let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
    let out2 = McpServers.run(&ctx2).await.unwrap();
    assert_eq!(out2.state, StepState::Done, "{out2:?}");
    // Phase 1 (§10.1): the step writes nothing — the retired
    // mcp-servers.json store must not be created, and the success
    // log notes that it is inert.
    assert!(
        ops2.file_bytes(&PathBuf::from("/data/mcp/mcp-servers.json"))
            .is_none(),
        "mcp-servers.json must no longer be written"
    );
    let log_text = out2.log_tail.join("\n");
    assert!(
        log_text.contains("inert") || log_text.contains("no longer written"),
        "{log_text}"
    );
}

#[tokio::test]
async fn mcp_servers_brave_search_missing_credential_fails() {
    // Phase 4 (§9): brave_search is Gateway-exposed with a
    // cred_ref, so its missing credential fails the step and
    // the failure names the server and its target.
    let brave = manager_config::BRAVE_SEARCH_MCP_CRED_REF;
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_credential(GITHUB_CRED_TARGET);
    ops.with_credential(GDRIVE_CRED_TARGET);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = McpServers.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("brave_search"), "{}", out.message);
    assert!(out.message.contains(brave), "{}", out.message);
    // The remediation names both ways out.
    let log_text = out.log_tail.join("\n");
    assert!(
        log_text.contains("store the credential") && log_text.contains("Disabled"),
        "{log_text}"
    );
    // Per-server log lines cover present and missing alike.
    assert!(
        log_text.contains("credential 'local-llm-service-manager/github-mcp' present"),
        "{log_text}"
    );
    assert!(
        log_text.contains(&format!("credential '{brave}' missing")),
        "{log_text}"
    );

    // With the Brave credential stored, the same registry passes.
    let ops2 = Arc::new(FakeSystemOps::new());
    ops2.with_credential(GITHUB_CRED_TARGET);
    ops2.with_credential(GDRIVE_CRED_TARGET);
    ops2.with_credential(brave);
    let ctx2 = ctx_with(ops2, RunMode::Install);
    let out2 = McpServers.run(&ctx2).await.unwrap();
    assert_eq!(out2.state, StepState::Done, "{out2:?}");
    assert!(out2.message.contains("brave_search"), "{}", out2.message);
}

#[tokio::test]
async fn mcp_servers_disabled_server_credential_not_checked() {
    // A Disabled server is not checked at all: its missing
    // credential cannot fail the step (design §9).
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_credential("local-llm-service-manager/github-mcp");
    let ctx = ctx_with_mcp_servers(
        ops,
        vec![
            mcp_test_server(
                "github",
                McpExposure::Local,
                Some("local-llm-service-manager/github-mcp"),
            ),
            mcp_test_server(
                "web_fetch",
                McpExposure::Disabled,
                Some("local-llm-service-manager/web-fetch"),
            ),
        ],
    );
    let out = McpServers.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("github"), "{}", out.message);
    assert!(!out.message.contains("web_fetch"), "{}", out.message);
}

#[tokio::test]
async fn mcp_servers_notes_retired_file_inert_when_present() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_credential(GITHUB_CRED_TARGET);
    ops.with_credential(GDRIVE_CRED_TARGET);
    ops.with_credential(manager_config::BRAVE_SEARCH_MCP_CRED_REF);
    // A pre-Phase-1 file left on disk.
    ops.with_file(PathBuf::from("/data/mcp/mcp-servers.json"), b"{}".to_vec());
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = McpServers.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let log_text = out.log_tail.join("\n");
    assert!(log_text.contains("inert"), "{log_text}");
    // The file is left in place, untouched.
    assert_eq!(
        ops.file_bytes(&PathBuf::from("/data/mcp/mcp-servers.json"))
            .unwrap(),
        b"{}".to_vec()
    );
}

// --- WinMlRegister ---

fn winml_ctx(ops: Arc<FakeSystemOps>, mode: RunMode, model_id: Option<&str>) -> WizardContext {
    let mut config = test_config_with_models();
    config.winml.enabled = true;
    config.winml.model_id = model_id.map(str::to_string);
    WizardContext::new(config, PathBuf::from("/data"), mode, ops)
}

#[tokio::test]
async fn winml_disabled_skips() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = ctx_with(ops, RunMode::Install); // winml disabled by default
    let out = WinMlRegister.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Skipped);
}

#[tokio::test]
async fn winml_registers_small_model_with_limits() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_http_get(
        "http://127.0.0.1:8090/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = winml_ctx(ops.clone(), RunMode::Install, Some("qwen3-8b"));
    let step = WinMlRegister;
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let raw = ops
        .file_bytes(&PathBuf::from("/data/winml/registration.json"))
        .expect("registration written");
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["model_id"], "qwen3-8b");
    assert_eq!(v["limits"]["supports_tensor_split"], false);
    assert_eq!(v["limits"]["experimental"], true);
}

#[tokio::test]
async fn winml_rejects_large_model_class() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = winml_ctx(ops, RunMode::Install, Some("qwen3-32b"));
    let out = WinMlRegister.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("ineligible"), "{}", out.message);
}

#[tokio::test]
async fn winml_fails_when_server_down() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = winml_ctx(ops, RunMode::Install, Some("qwen3-8b"));
    let out = WinMlRegister.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("not reachable"));
    assert!(WinMlRegister
        .remediation_hint()
        .unwrap()
        .contains("WinMLServer"));
}

// --- MxcPolicies ---

#[tokio::test]
async fn mxc_policy_written_validated_and_self_tested() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command(
        &["wxc-exec.exe", "--probe"],
        FakeSystemOps::ok_output(r#"{"psec":"1.1","ingress_loopback":true}"#),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = MxcPolicies;
    assert_eq!(step.planned_commands(&ctx).len(), 2);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("Learning"), "{}", out.message);
    let raw = ops
        .file_bytes(&PathBuf::from("/data/mxc/policy.json"))
        .expect("policy written");
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["default_deny"], true);
    // Self-test ran the pinned policy in --dry-run mode (the
    // 1.0.0 CLI has no --policy/--self-test form).
    let executed = ops.executed_commands();
    assert!(executed
        .iter()
        .any(|a| { a.contains(&"--config".to_string()) && a.contains(&"--dry-run".to_string()) }));
}

#[tokio::test]
async fn mxc_audit_revalidates_policy() {
    let ops = Arc::new(FakeSystemOps::new());
    let install_ctx = ctx_with(ops.clone(), RunMode::Install);
    assert_eq!(
        MxcPolicies.run(&install_ctx).await.unwrap().state,
        StepState::Done
    );
    let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = MxcPolicies.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
}

#[tokio::test]
async fn mxc_missing_runtime_skips() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command_failure(
        &["wxc-exec.exe", "--probe"],
        -1,
        "spawn failed: program not found",
    );
    let ctx = ctx_with(ops, RunMode::Install);
    let out = MxcPolicies.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Skipped, "{out:?}");
    assert!(out.message.contains("not installed"), "{}", out.message);
}

#[tokio::test]
async fn mxc_probe_failure_fails_step() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_command_failure(&["wxc-exec.exe", "--probe"], 1, "not found");
    let ctx = ctx_with(ops, RunMode::Install);
    let out = MxcPolicies.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("not available"), "{}", out.message);
}

// --- InstallMxc ---

pub(super) fn mxc_pin_for(tarball: &[u8], exec: &[u8], prep: &[u8]) -> MxcSdkPin {
    MxcSdkPin {
        version: "1.0.0".to_string(),
        tarball_asset: "mxc-sdk-1.0.0.tgz".to_string(),
        tarball_sha256: sha256_hex(tarball),
        tarball_size: tarball.len() as u64,
        wxc_exec_sha256: sha256_hex(exec),
        wxc_host_prep_sha256: sha256_hex(prep),
    }
}

#[tokio::test]
async fn install_mxc_downloads_extracts_verifies_and_preps() {
    let ops = Arc::new(FakeSystemOps::new());
    let tarball = b"fake-tarball-bytes";
    let exec = b"fake-wxc-exec";
    let prep = b"fake-wxc-host-prep";
    let pin = mxc_pin_for(tarball, exec, prep);
    ops.with_download(&pin.tarball_url(), tarball.to_vec());
    ops.with_extracted_file(
        PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
        exec.to_vec(),
    );
    ops.with_extracted_file(
        PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
        prep.to_vec(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallMxc::with_pin(pin.clone()).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.message.contains("host preparation complete"),
        "{}",
        out.message
    );
    assert_eq!(ops.downloaded_to().len(), 1);
    let extractions = ops.tar_gz_extractions();
    assert_eq!(extractions.len(), 1);
    assert_eq!(extractions[0].2, "package/bin/x64/");
    assert_eq!(extractions[0].3, "package/");
    let elevated = ops.elevated_calls();
    assert_eq!(elevated.len(), 1);
    assert_eq!(elevated[0][0], "cmd.exe");
    let marker = ops
        .file_bytes(&PathBuf::from("/data/mxc/.installed.json"))
        .expect("marker written");
    let v: serde_json::Value = serde_json::from_slice(&marker).unwrap();
    assert_eq!(v["host_prep"]["system_drive"], true);
    // A re-run is a no-op: no second download, no second prompt.
    let out2 = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out2.state, StepState::Done, "{out2:?}");
    assert_eq!(ops.downloaded_to().len(), 1);
    assert_eq!(ops.elevated_calls().len(), 1);
}

#[tokio::test]
async fn install_mxc_already_installed_skips_download() {
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
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(ops.downloaded_to().is_empty());
    assert!(ops.tar_gz_extractions().is_empty());
    assert_eq!(ops.elevated_calls().len(), 1);
}

#[tokio::test]
async fn install_mxc_tarball_checksum_mismatch_fails_closed() {
    let ops = Arc::new(FakeSystemOps::new());
    let pin = mxc_pin_for(b"the-real-tarball", b"e", b"p");
    ops.with_download(&pin.tarball_url(), b"tampered-bytes".to_vec());
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("checksum mismatch"), "{}", out.message);
    assert!(ops.tar_gz_extractions().is_empty());
}

// --- InstallPostgres ---
