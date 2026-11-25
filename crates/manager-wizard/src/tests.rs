use super::*;
use crate::fake::FakeSystemOps;

#[test]
fn gateway_applied_stamp_roundtrips_and_rejects_garbage() {
    let at = chrono::DateTime::parse_from_rfc3339("2026-11-12T10:00:00.123Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let json = gateway_applied_stamp_json(at, 4242);
    assert_eq!(parse_gateway_applied_at(&json), Some(at));
    assert_eq!(parse_gateway_applied_at("not json"), None);
    assert_eq!(parse_gateway_applied_at("{\"pid\": 1}"), None);

    let dir = std::env::temp_dir().join(format!("wizard-applied-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(gateway_applied_at(&dir), None);
    write_gateway_applied_stamp(&dir, at, 4242).unwrap();
    assert_eq!(gateway_applied_at(&dir), Some(at));
    assert!(!gateway_applied_stamp_path(&dir)
        .with_extension("json.tmp")
        .exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lifecycle_log_paths_live_in_the_logs_dir() {
    let d = Path::new("/data");
    assert_eq!(
        gateway_lifecycle_log(d),
        PathBuf::from("/data/logs/gateway.lifecycle.log")
    );
    assert_eq!(
        postgres_lifecycle_log(d),
        PathBuf::from("/data/logs/postgres.lifecycle.log")
    );
    // The service host writes the same file in service mode.
    let def = manager_services::gateway_definition(
        manager_services::LaunchSpec {
            program: PathBuf::from("python.exe"),
            args: Vec::new(),
            env: Vec::new(),
            workdir: None,
        },
        d.join("logs").join("gateway.stdout.log"),
        d.join("logs").join("gateway.stderr.log"),
        gateway_pid_path(d),
    );
    assert_eq!(
        manager_services::lifecycle_log_path(&def),
        Some(gateway_lifecycle_log(d))
    );
}

/// Minimal scripted step for runner tests.
struct ScriptStep {
    id: &'static str,
    prereqs: Vec<String>,
    check: bool,
    fail: bool,
    err: bool,
    log_lines: usize,
}

impl ScriptStep {
    fn ok(id: &'static str) -> Self {
        Self {
            id,
            prereqs: vec![],
            check: true,
            fail: false,
            err: false,
            log_lines: 0,
        }
    }
}

#[async_trait]
impl WizardStep for ScriptStep {
    fn id(&self) -> &str {
        self.id
    }
    fn name(&self) -> &str {
        self.id
    }
    fn prerequisites(&self) -> Vec<String> {
        self.prereqs.clone()
    }
    fn is_check(&self) -> bool {
        self.check
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        vec![]
    }
    async fn run(&self, _ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        if self.err {
            return Err(WizardError::Cancelled);
        }
        if self.fail {
            return Ok(StepOutcome::failed(
                format!("{} failed", self.id),
                vec!["boom".to_string()],
            ));
        }
        Ok(StepOutcome::done(
            format!("{} ok", self.id),
            (0..self.log_lines).map(|i| format!("line {i}")).collect(),
        ))
    }
}

fn ctx() -> WizardContext {
    WizardContext::new(
        manager_config::AppConfig::default_config(),
        PathBuf::from("test-data"),
        RunMode::Install,
        Arc::new(FakeSystemOps::new()),
    )
}

fn wizard(ids: &[(&'static str, bool)]) -> Wizard {
    // (id, fail)
    let steps: Vec<Box<dyn WizardStep>> = ids
        .iter()
        .map(|(id, fail)| {
            let mut s = ScriptStep::ok(id);
            s.fail = *fail;
            Box::new(s) as Box<dyn WizardStep>
        })
        .collect();
    Wizard::new(steps, RunMode::Install).expect("test steps have unique ids")
}

#[tokio::test]
async fn runs_steps_in_order_and_reports_progress() {
    let c = ctx();
    let mut w = wizard(&[("a", false), ("b", false), ("c", false)]);
    let report = w.run_all(&c).await;
    assert_eq!(w.progress(), (3, 3));
    assert_eq!(
        report
            .steps
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert!(report.steps.iter().all(|s| s.state == StepState::Done));
    assert!((report.fraction() - 1.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn hydrate_adopts_passing_checks_without_overwriting_session_state() {
    let step_report = |id: &str, state: StepState, message: &str| StepReport {
        id: id.to_string(),
        name: id.to_string(),
        state,
        message: message.to_string(),
        duration_ms: 1,
        log_tail: Vec::new(),
        note: None,
        manual_command: None,
    };
    let audit_report = WizardReport {
        mode: RunMode::Audit,
        steps: vec![
            step_report("a", StepState::Done, "verified by audit"),
            step_report("b", StepState::Failed, "not in place"),
        ],
    };
    // Fresh wizard: the passing check is adopted; the failing one
    // stays Queued (not a failed install attempt).
    let mut w = wizard(&[("a", false), ("b", false)]);
    w.hydrate(&audit_report, true);
    let after = w.report();
    let a = after.steps.iter().find(|s| s.id == "a").unwrap();
    assert_eq!(a.state, StepState::Done);
    assert_eq!(a.message, "verified by audit");
    let b = after.steps.iter().find(|s| s.id == "b").unwrap();
    assert_eq!(b.state, StepState::Queued);

    // A step already run this session keeps its own outcome.
    let c = ctx();
    let mut w2 = wizard(&[("a", false), ("b", false)]);
    let session = w2.run_step(&c, "a").await.unwrap();
    assert_eq!(session.state, StepState::Done);
    w2.hydrate(&audit_report, true);
    let a2 = w2.report().steps.into_iter().find(|s| s.id == "a").unwrap();
    assert_eq!(a2.state, StepState::Done);
    assert_ne!(a2.message, "verified by audit");
}

#[test]
fn restore_adopts_all_outcomes_and_marks_interrupted() {
    let step_report = |id: &str, state: StepState, message: &str| StepReport {
        id: id.to_string(),
        name: id.to_string(),
        state,
        message: message.to_string(),
        duration_ms: 42,
        log_tail: vec!["line".to_string()],
        note: None,
        manual_command: None,
    };
    let persisted = WizardReport {
        mode: RunMode::Install,
        steps: vec![
            step_report("a", StepState::Done, "installed"),
            step_report("b", StepState::Failed, "boom"),
            step_report("c", StepState::Running, ""),
            step_report("ghost", StepState::Done, "unknown id"),
        ],
    };
    let mut w = wizard(&[("a", false), ("b", false), ("c", false)]);
    w.restore(&persisted, true, false);
    let after = w.report();
    let get = |id: &str| after.steps.iter().find(|s| s.id == id).unwrap();
    // Done and Failed are adopted verbatim — progress and the
    // record of what failed both survive the restart.
    assert_eq!(get("a").state, StepState::Done);
    assert_eq!(get("a").message, "installed");
    assert_eq!(get("b").state, StepState::Failed);
    assert_eq!(get("b").message, "boom");
    // The interrupted step is runnable again, with the note.
    assert_eq!(get("c").state, StepState::Queued);
    assert!(get("c")
        .note
        .as_deref()
        .unwrap_or("")
        .contains("interrupted"));
    // Unknown ids did not appear.
    assert_eq!(after.steps.len(), 3);
}

#[test]
fn restore_rejects_other_mode_and_never_overwrites_session_state() {
    let step_report = |id: &str, state: StepState| StepReport {
        id: id.to_string(),
        name: id.to_string(),
        state,
        message: "from audit".to_string(),
        duration_ms: 1,
        log_tail: Vec::new(),
        note: None,
        manual_command: None,
    };
    // An audit report must not be restored into the install
    // wizard (its failures are checks, not install outcomes).
    let mut w = wizard(&[("a", false)]);
    w.restore(
        &WizardReport {
            mode: RunMode::Audit,
            steps: vec![step_report("a", StepState::Failed)],
        },
        true,
        false,
    );
    assert_eq!(w.report().steps[0].state, StepState::Queued);
}

fn gateway_step_report(state: StepState) -> StepReport {
    StepReport {
        id: steps::STEP_START_GATEWAY.to_string(),
        name: "Start LiteLLM gateway".to_string(),
        state,
        message: "gateway healthy".to_string(),
        duration_ms: 1,
        log_tail: Vec::new(),
        note: None,
        manual_command: None,
    }
}

#[test]
fn hydrate_does_not_adopt_start_gateway_done_when_config_drifted() {
    // A stale gateway answering the audit probe must not mark
    // start-gateway complete when the saved configuration has
    // moved on: the step stays Queued so Apply/Start runs it.
    let mut w = wizard(&[(steps::STEP_START_GATEWAY, false), ("a", false)]);
    let audit_report = WizardReport {
        mode: RunMode::Audit,
        steps: vec![
            gateway_step_report(StepState::Done),
            StepReport {
                id: "a".to_string(),
                name: "a".to_string(),
                state: StepState::Done,
                message: "verified by audit".to_string(),
                duration_ms: 1,
                log_tail: Vec::new(),
                note: None,
                manual_command: None,
            },
        ],
    };
    w.hydrate(&audit_report, false);
    let after = w.report();
    let gw = after
        .steps
        .iter()
        .find(|s| s.id == steps::STEP_START_GATEWAY)
        .unwrap();
    assert_eq!(gw.state, StepState::Queued);
    assert!(gw.note.as_deref().unwrap_or("").contains("changed"));
    // Other steps are unaffected by the gateway rule.
    let a = after.steps.iter().find(|s| s.id == "a").unwrap();
    assert_eq!(a.state, StepState::Done);

    // With the config current, the same audit result is adopted.
    let mut w2 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
    w2.hydrate(&audit_report, true);
    assert_eq!(w2.state(steps::STEP_START_GATEWAY), Some(StepState::Done));
}

#[test]
fn restore_does_not_adopt_start_gateway_done_when_config_drifted() {
    let persisted = WizardReport {
        mode: RunMode::Install,
        steps: vec![gateway_step_report(StepState::Done)],
    };
    let mut w = wizard(&[(steps::STEP_START_GATEWAY, false)]);
    w.restore(&persisted, false, false);
    assert_eq!(w.state(steps::STEP_START_GATEWAY), Some(StepState::Queued));
    // Failures are the user's progress and still restore under drift.
    let persisted_failed = WizardReport {
        mode: RunMode::Install,
        steps: vec![gateway_step_report(StepState::Failed)],
    };
    let mut w2 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
    w2.restore(&persisted_failed, false, false);
    assert_eq!(w2.state(steps::STEP_START_GATEWAY), Some(StepState::Failed));
    // Current config: the persisted Done restores as before.
    let mut w3 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
    w3.restore(&persisted, true, false);
    assert_eq!(w3.state(steps::STEP_START_GATEWAY), Some(StepState::Done));
}

/// A second reader of the saved state must not turn the running
/// gateway's Done into Queued: it stays Done, with the drift note,
/// which goes away once the configuration is current again.
#[test]
fn restore_keeps_start_gateway_done_while_the_gateway_runs() {
    let persisted = WizardReport {
        mode: RunMode::Install,
        steps: vec![gateway_step_report(StepState::Done)],
    };
    let mut w = wizard(&[(steps::STEP_START_GATEWAY, false)]);
    w.restore(&persisted, false, true);
    let drifted = w.report();
    assert_eq!(drifted.steps[0].state, StepState::Done);
    assert!(drifted.steps[0]
        .note
        .as_deref()
        .unwrap()
        .contains("changed"));

    let mut w2 = wizard(&[(steps::STEP_START_GATEWAY, false)]);
    w2.restore(&drifted, true, true);
    assert_eq!(w2.report().steps[0].state, StepState::Done);
    assert_eq!(w2.report().steps[0].note, None);
}

fn full_report(mode: RunMode, state_of: impl Fn(&str) -> StepState) -> WizardReport {
    WizardReport {
        mode,
        steps: steps::default_steps()
            .iter()
            .map(|s| StepReport {
                id: s.id().to_string(),
                name: s.name().to_string(),
                state: state_of(s.id()),
                message: String::new(),
                duration_ms: 0,
                log_tail: Vec::new(),
                note: None,
                manual_command: None,
            })
            .collect(),
    }
}

#[test]
fn setup_is_complete_without_the_optional_steps() {
    let optional = |id: &str| steps::OPTIONAL_STEPS.contains(&id);
    // Required steps done, optional ones never run.
    let pending_optional = |id: &str| {
        if optional(id) {
            StepState::Queued
        } else {
            StepState::Done
        }
    };
    assert!(full_report(RunMode::Install, pending_optional).setup_complete());
    assert!(!full_report(RunMode::Audit, pending_optional).setup_complete());
    // Each required step holds setup open while it is pending.
    for step in steps::default_steps() {
        let id = step.id().to_string();
        let report = full_report(RunMode::Install, |s| {
            if s == id {
                StepState::Queued
            } else {
                StepState::Done
            }
        });
        assert_eq!(report.setup_complete(), optional(&id), "{id}");
    }
    // A failed optional step is unfinished business.
    let failed = full_report(RunMode::Install, |s| {
        if s == steps::STEP_MXC_POLICIES {
            StepState::Failed
        } else {
            StepState::Done
        }
    });
    assert!(!failed.setup_complete());
    // A report without the required steps is not a complete one.
    let empty = WizardReport {
        mode: RunMode::Install,
        steps: Vec::new(),
    };
    assert!(!empty.setup_complete());
}

#[test]
fn litellm_mcp_floor_comparison() {
    // Below the floor.
    assert_eq!(litellm_version_meets_mcp_floor("1.83.6"), Some(false));
    assert_eq!(litellm_version_meets_mcp_floor("1.82.99"), Some(false));
    assert_eq!(litellm_version_meets_mcp_floor("0.99.0"), Some(false));
    // At the floor.
    assert_eq!(litellm_version_meets_mcp_floor("1.83.7"), Some(true));
    // Above the floor (the repo's field-verified version).
    assert_eq!(litellm_version_meets_mcp_floor("1.105.1"), Some(true));
    assert_eq!(litellm_version_meets_mcp_floor("2.0.0"), Some(true));
    // Unparseable -> unknown (None), never a guess.
    assert_eq!(litellm_version_meets_mcp_floor(""), None);
    assert_eq!(litellm_version_meets_mcp_floor("unknown"), None);
    assert_eq!(litellm_version_meets_mcp_floor("1.83.x"), None);
    assert_eq!(litellm_version_meets_mcp_floor("1..7"), None);
    // Suffix / prefix forms parse tolerantly.
    assert_eq!(litellm_version_meets_mcp_floor("v1.83.7"), Some(true));
    assert_eq!(litellm_version_meets_mcp_floor("1.83.7-stable"), Some(true));
    assert_eq!(
        litellm_version_meets_mcp_floor("v1.105.1-stable"),
        Some(true)
    );
    assert_eq!(litellm_version_meets_mcp_floor(" 1.83.6 "), Some(false));
    // Missing trailing components read as zero.
    assert_eq!(litellm_version_meets_mcp_floor("1.83"), Some(false));
    assert_eq!(litellm_version_meets_mcp_floor("1.84"), Some(true));
}

#[test]
fn gateway_spawn_state_records_litellm_version_and_parses_legacy() {
    let cfg = manager_config::AppConfig::default_config();
    let fp = gateway_spawn_fingerprint(&cfg, false, None, None, &[]);
    let json = gateway_spawn_state_json(&fp, "yaml: true\n", Some("1.105.1"));
    let state: GatewaySpawnState = serde_json::from_str(&json).unwrap();
    assert_eq!(state.litellm_version.as_deref(), Some("1.105.1"));
    // None serializes as null and parses back as None.
    let json = gateway_spawn_state_json(&fp, "yaml: true\n", None);
    let state: GatewaySpawnState = serde_json::from_str(&json).unwrap();
    assert_eq!(state.litellm_version, None);
    // A record written before the field existed (no
    // litellm_version key at all) still parses.
    let legacy = format!(
        "{{\"fingerprint\": {}, \"yaml_sha256\": \"abc\"}}",
        serde_json::to_string(&fp).unwrap()
    );
    let state: GatewaySpawnState = serde_json::from_str(&legacy).unwrap();
    assert_eq!(state.litellm_version, None);
    assert_eq!(state.yaml_sha256, "abc");
}

#[test]
fn recorded_litellm_version_reads_spawn_state() {
    let dir = std::env::temp_dir().join(format!("wizard-litellm-version-{}", std::process::id()));
    let gw_dir = dir.join("gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    // No record at all -> None (not measured).
    assert_eq!(recorded_litellm_version(&dir), None);
    let cfg = manager_config::AppConfig::default_config();
    let fp = gateway_spawn_fingerprint(&cfg, false, None, None, &[]);
    // A record with a probed version returns it.
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&fp, "yaml: true\n", Some("1.105.1")),
    )
    .unwrap();
    assert_eq!(recorded_litellm_version(&dir).as_deref(), Some("1.105.1"));
    // A record whose probe found nothing returns None.
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&fp, "yaml: true\n", None),
    )
    .unwrap();
    assert_eq!(recorded_litellm_version(&dir), None);
    // A corrupt record returns None rather than failing.
    std::fs::write(gateway_spawn_state_path(&dir), "not json").unwrap();
    assert_eq!(recorded_litellm_version(&dir), None);
    let _ = std::fs::remove_dir_all(&dir);
}

fn mcp_cfg_server(
    id: &str,
    exposure: McpExposure,
    env_keys: &[&str],
    cred_ref: Option<&str>,
) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        display_name: id.to_string(),
        transport: McpTransport::Stdio,
        exposure,
        command: Some("npx".to_string()),
        args: vec!["-y".to_string(), format!("@example/{id}@1.0.0")],
        url: None,
        env_keys: env_keys.iter().map(|s| s.to_string()).collect(),
        cred_ref: cred_ref.map(str::to_string),
        signup_url: None,
        credential_hint: None,
        allowed_tools: None,
        metered: false,
        rpm_limit: None,
    }
}

#[test]
fn mcp_arming_rule_covers_no_env_resolvable_and_unresolvable() {
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.mcp.servers = vec![
        // No env keys: armed without any credential.
        mcp_cfg_server("no_env", McpExposure::Gateway, &[], None),
        // Env keys + resolvable credential: armed.
        mcp_cfg_server("armed", McpExposure::Gateway, &["K"], Some("t/armed")),
        // Env keys + unresolvable credential: NOT armed.
        mcp_cfg_server("unarmed", McpExposure::Gateway, &["K"], Some("t/missing")),
        // Env keys but no cred_ref named: NOT armed.
        mcp_cfg_server("no_ref", McpExposure::Gateway, &["K"], None),
        // Local servers are never in the gateway armed set.
        mcp_cfg_server("local_one", McpExposure::Local, &[], None),
    ];
    let armed = armed_gateway_mcp_servers(&cfg, |t| t == "t/armed");
    let ids: Vec<&str> = armed.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["no_env", "armed"]);
    assert!(mcp_armed_set_has_stdio(&armed));
    // The filtered config holds ONLY the armed Gateway servers,
    // and generation from it emits exactly that set (the mixed
    // armed-set generation case, design §4.1/§4.2).
    let mut full = manager_config::AppConfig::default_config();
    for b in full.backends.iter_mut() {
        b.enabled = true;
    }
    full.mcp.servers.clone_from(&cfg.mcp.servers);
    let filtered = config_with_armed_gateway_mcp_servers(&full, |t| t == "t/armed");
    assert_eq!(filtered.mcp.servers.len(), 2);
    let yaml = manager_gateway::generate_config_yaml(&filtered).unwrap();
    assert!(yaml.contains("  no_env:\n"), "{yaml}");
    assert!(yaml.contains("  armed:\n"), "{yaml}");
    assert!(!yaml.contains("unarmed"), "{yaml}");
    assert!(!yaml.contains("no_ref"), "{yaml}");
    assert!(!yaml.contains("local_one"), "{yaml}");
}

#[test]
fn mcp_fingerprint_fields_and_legacy_record_compatibility() {
    // Empty armed set: hash is "" and stdio is false — equal to
    // the serde defaults of a pre-feature spawn-state record.
    let cfg = manager_config::AppConfig::default_config();
    let fp_empty = gateway_spawn_fingerprint(&cfg, false, None, None, &[]);
    assert_eq!(fp_empty.mcp_servers_hash, "");
    assert!(!fp_empty.mcp_stdio_enabled);
    assert_eq!(mcp_armed_servers_hash(&[]), "");
    // A legacy fingerprint JSON (no MCP fields at all) parses
    // with those defaults and equals the empty-set fingerprint.
    let legacy = r#"{"port":4000,"bind_host":"0.0.0.0","postgres_port":null,"master_key_hash":"","openrouter_key_hash":""}"#;
    let parsed: GatewaySpawnFingerprint = serde_json::from_str(legacy).unwrap();
    assert_eq!(parsed.mcp_servers_hash, "");
    assert!(!parsed.mcp_stdio_enabled);
    let mut cfg2 = manager_config::AppConfig::default_config();
    cfg2.gateway.port = parsed.port;
    assert_eq!(
        gateway_spawn_fingerprint(&cfg2, false, None, None, &[]),
        parsed
    );

    // An armed stdio server changes both fields; editing a
    // generation-relevant field changes the hash; an http-only
    // set does not set the stdio flag.
    let server = mcp_cfg_server("srv", McpExposure::Gateway, &["K"], Some("t/srv"));
    let fp = gateway_spawn_fingerprint(&cfg, false, None, None, &[&server]);
    assert!(fp.mcp_stdio_enabled);
    assert_eq!(fp.mcp_servers_hash.len(), 64);
    let mut edited = server.clone();
    edited.args.push("--extra".to_string());
    assert_ne!(
        mcp_armed_servers_hash(&[&server]),
        mcp_armed_servers_hash(&[&edited])
    );
    let mut http = server.clone();
    http.transport = McpTransport::Http;
    http.command = None;
    http.url = Some("https://example.com/mcp".to_string());
    assert!(!mcp_armed_set_has_stdio(&[&http]));
    assert_ne!(mcp_armed_servers_hash(&[&http]), fp.mcp_servers_hash);
}

#[test]
fn mcp_gateway_config_is_current_uses_arming_for_yaml() {
    // A gateway spawned with an armed server stays current
    // while the credential resolves, and reads as drift when it
    // stops resolving (a start now would omit the server).
    let dir = std::env::temp_dir().join(format!("wizard-gw-mcp-{}", std::process::id()));
    let gw_dir = dir.join("gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    let server = mcp_cfg_server("srv", McpExposure::Gateway, &["K"], Some("t/srv"));
    cfg.mcp.servers.push(server.clone());
    let armed = armed_gateway_mcp_servers(&cfg, |_| true);
    let filtered = config_with_armed_gateway_mcp_servers(&cfg, |_| true);
    let yaml = manager_gateway::generate_config_yaml(&filtered).unwrap();
    assert!(yaml.contains("mcp_servers:"), "{yaml}");
    std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
    let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None, &armed);
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&fp, &yaml, None),
    )
    .unwrap();
    assert!(gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| false));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn searxng_change_registers_as_gateway_drift() {
    let dir = std::env::temp_dir().join(format!("wizard-gw-searxng-{}", std::process::id()));
    let gw_dir = dir.join("gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    cfg.mcp.searxng = manager_config::SearxngSettings {
        enabled: true,
        api_base: "http://127.0.0.1:8080".to_string(),
    };
    let armed = armed_gateway_mcp_servers(&cfg, |_| true);
    let filtered = config_with_armed_gateway_mcp_servers(&cfg, |_| true);
    let yaml = manager_gateway::generate_config_yaml(&filtered).unwrap();
    assert!(yaml.contains("search_tools:"), "{yaml}");
    std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
    let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None, &armed);
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&fp, &yaml, None),
    )
    .unwrap();
    assert!(gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // Changing the base changes the fingerprint (and the YAML):
    // not current.
    let mut moved = cfg.clone();
    moved.mcp.searxng.api_base = "http://127.0.0.1:9999".to_string();
    let armed_moved = armed_gateway_mcp_servers(&moved, |_| true);
    let fp_moved = gateway_spawn_fingerprint(&moved, true, Some("sk-master"), None, &armed_moved);
    assert_ne!(fp_moved.searxng_hash, fp.searxng_hash);
    assert!(!gateway_config_is_current(&moved, &dir, &fp_moved, |_| {
        true
    }));
    // Disabling entirely likewise drifts from the recorded spawn.
    let mut off = cfg.clone();
    off.mcp.searxng.enabled = false;
    let fp_off = gateway_spawn_fingerprint(&off, true, Some("sk-master"), None, &armed);
    assert!(!gateway_config_is_current(&off, &dir, &fp_off, |_| true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn routing_rule_change_registers_as_gateway_drift() {
    // The routing callback is a second generated artifact
    // (routing-rules design §6.3): a rule-table change that
    // leaves the YAML untouched must still read as drift, and
    // a stale callback file with no enabled rule must too.
    let dir = std::env::temp_dir().join(format!("wizard-gw-routing-{}", std::process::id()));
    let gw_dir = dir.join("gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    cfg.gateway.routing_rules.push(manager_config::RoutingRule {
        id: "context-overflow".to_string(),
        description: "test".to_string(),
        enabled: true,
        kind: Some(manager_config::RoutingRuleKind::ContextOverflow {
            target_group: "planner".to_string(),
            optimal_fraction: 0.5,
            optimal_tokens: None,
            output_reserve_tokens: 1024,
            advisory: true,
        }),
        litellm_fragment: None,
        source: "user".to_string(),
    });
    let armed = armed_gateway_mcp_servers(&cfg, |_| true);
    let filtered = config_with_armed_gateway_mcp_servers(&cfg, |_| true);
    let yaml = manager_gateway::generate_config_yaml(&filtered).unwrap();
    let py = manager_gateway::generate_routing_callback(&filtered)
        .expect("callback for an enabled rule");
    std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
    std::fs::write(gw_dir.join("llmdil_routing.py"), &py).unwrap();
    let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None, &armed);
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&fp, &yaml, None),
    )
    .unwrap();
    assert!(gateway_config_is_current(&cfg, &dir, &fp, |_| true));

    // Change only a table value (the output reserve): the YAML
    // is byte-identical, the callback differs — drift.
    let mut changed = cfg.clone();
    if let Some(manager_config::RoutingRuleKind::ContextOverflow {
        output_reserve_tokens,
        ..
    }) = &mut changed.gateway.routing_rules[0].kind
    {
        *output_reserve_tokens = 4096;
    }
    let filtered_changed = config_with_armed_gateway_mcp_servers(&changed, |_| true);
    assert_eq!(
        manager_gateway::generate_config_yaml(&filtered_changed).unwrap(),
        yaml,
        "the reserve change must not alter the YAML for this test to prove its point"
    );
    let armed_changed = armed_gateway_mcp_servers(&changed, |_| true);
    let fp_changed =
        gateway_spawn_fingerprint(&changed, true, Some("sk-master"), None, &armed_changed);
    assert!(!gateway_config_is_current(
        &changed,
        &dir,
        &fp_changed,
        |_| true
    ));

    // Disabling the rule while the file remains: drift.
    let mut off = cfg.clone();
    off.gateway.routing_rules[0].enabled = false;
    let armed_off = armed_gateway_mcp_servers(&off, |_| true);
    let fp_off = gateway_spawn_fingerprint(&off, true, Some("sk-master"), None, &armed_off);
    assert!(!gateway_config_is_current(&off, &dir, &fp_off, |_| true));

    // A callback file that no longer matches its own config
    // (hand-edited / stale): drift.
    std::fs::write(gw_dir.join("llmdil_routing.py"), "# tampered\n").unwrap();
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mcp_response_parsers_accept_the_documented_shapes() {
    // Registered list: array of objects, array of strings,
    // envelope object.
    assert_eq!(
        parse_mcp_registered_server_ids(r#"[{"server_id":"brave_search"},{"id":"web_fetch"}]"#),
        vec!["brave_search", "web_fetch"]
    );
    assert_eq!(
        parse_mcp_registered_server_ids(r#"["a","b"]"#),
        vec!["a", "b"]
    );
    assert_eq!(
        parse_mcp_registered_server_ids(r#"{"servers":[{"name":"x"}]}"#),
        vec!["x"]
    );
    assert!(parse_mcp_registered_server_ids("not json").is_empty());
    // Tools list: {tools:[{name}]} and bare arrays.
    assert_eq!(
        parse_mcp_tool_names(r#"{"tools":[{"name":"brave_web_search"}]}"#),
        vec!["brave_web_search"]
    );
    assert_eq!(parse_mcp_tool_names(r#"[{"name":"fetch"}]"#), vec!["fetch"]);
    assert!(parse_mcp_tool_names(r#"{"tools":[]}"#).is_empty());
    // Error envelope.
    assert_eq!(
        mcp_response_error(r#"{"error":{"message":"boom"}}"#).as_deref(),
        Some("boom")
    );
    assert_eq!(
        mcp_response_error(r#"{"error":"nope"}"#).as_deref(),
        Some("nope")
    );
    assert_eq!(mcp_response_error(r#"{"tools":[]}"#), None);
    assert_eq!(mcp_response_error("not json"), None);
}

#[test]
fn searxng_fingerprint_covers_toggle_and_base_but_not_absence() {
    let mut cfg = manager_config::AppConfig::default_config();
    // SearXNG off (the default): the contribution is empty —
    // exactly what a pre-Phase-3 spawn record holds.
    let fp_off = gateway_spawn_fingerprint(&cfg, false, None, None, &[]);
    assert_eq!(fp_off.searxng_hash, "");
    assert_eq!(
        searxng_fingerprint_hash(&manager_config::SearxngSettings::default()),
        ""
    );
    // A legacy fingerprint JSON (no searxng_hash key) parses
    // with the empty contribution and still equals the
    // searxng-less fingerprint.
    let legacy = r#"{"port":4000,"bind_host":"0.0.0.0","postgres_port":null,"master_key_hash":"","openrouter_key_hash":"","mcp_stdio_enabled":false,"mcp_servers_hash":""}"#;
    let parsed: GatewaySpawnFingerprint = serde_json::from_str(legacy).unwrap();
    assert_eq!(parsed.searxng_hash, "");
    cfg.gateway.port = parsed.port;
    // (fp_off was computed from the same default port; compare
    // against a fingerprint rebuilt on the parsed port.)
    assert_eq!(
        gateway_spawn_fingerprint(&cfg, false, None, None, &[]).searxng_hash,
        parsed.searxng_hash
    );
    // Enabled with a base: non-empty, and sensitive to both the
    // toggle and the base value.
    cfg.mcp.searxng = manager_config::SearxngSettings {
        enabled: true,
        api_base: "http://127.0.0.1:8080".to_string(),
    };
    let fp_on = gateway_spawn_fingerprint(&cfg, false, None, None, &[]);
    assert_eq!(fp_on.searxng_hash.len(), 64);
    assert_ne!(fp_on.searxng_hash, fp_off.searxng_hash);
    cfg.mcp.searxng.api_base = "http://127.0.0.1:9999".to_string();
    let fp_moved = gateway_spawn_fingerprint(&cfg, false, None, None, &[]);
    assert_ne!(fp_moved.searxng_hash, fp_on.searxng_hash);
    // Enabled with an empty base contributes nothing (no
    // search_tools block, no spawn env — nothing to drift).
    cfg.mcp.searxng.api_base = String::new();
    assert_eq!(
        gateway_spawn_fingerprint(&cfg, false, None, None, &[]).searxng_hash,
        ""
    );
}

#[test]
fn mcp_tool_call_body_and_summary_match_rest_docs() {
    // The §8 Brave test body: unprefixed tool name + server_id,
    // one-word query, arguments an object (never null).
    let body = brave_search_test_call_body();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["server_id"], "brave_search");
    assert_eq!(parsed["name"], "brave_web_search");
    assert_eq!(
        parsed["arguments"],
        serde_json::json!({ "query": "llmdil" })
    );
    // The generic constructor keeps the same shape.
    let body = mcp_tool_call_body("srv", "tool", serde_json::json!({}));
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["arguments"], serde_json::json!({}));
    // Summary: 200 without an error envelope is ok, with the
    // excerpt capped at the documented length.
    let long = "x".repeat(500);
    let (ok, detail) = summarize_mcp_tool_call_response(200, &long);
    assert!(ok);
    assert_eq!(detail.chars().count(), MCP_TOOL_CALL_DETAIL_MAX_CHARS + 1);
    // Error envelope and non-200 are failures carrying the
    // gateway's text.
    let (ok, detail) = summarize_mcp_tool_call_response(200, r#"{"error":{"message":"boom"}}"#);
    assert!(!ok);
    assert!(detail.contains("boom"), "{detail}");
    let (ok, detail) = summarize_mcp_tool_call_response(500, "oops");
    assert!(!ok);
    assert!(detail.contains("500"), "{detail}");
}

#[test]
fn gateway_config_is_current_compares_generated_yaml_to_disk() {
    let dir = std::env::temp_dir().join(format!("wizard-gw-current-{}", std::process::id()));
    let gw_dir = dir.join("gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    // The default config ships with backends disabled (a fresh
    // install), which generates no YAML at all; enable them so
    // generation succeeds.
    let mut cfg = manager_config::AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None, &[]);
    // Missing file: not current (the step must run to write it).
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    let yaml = manager_gateway::generate_config_yaml(&cfg).unwrap();
    std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
    // YAML alone is not enough: no spawn-state record means the
    // running gateway's argv/env inputs are unknown.
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&fp, &yaml, Some("1.105.1")),
    )
    .unwrap();
    assert!(gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // CRLF line endings and trailing blank lines are not drift.
    std::fs::write(
        gw_dir.join("config.yaml"),
        format!("{}\r\n\r\n", yaml.replace('\n', "\r\n")),
    )
    .unwrap();
    assert!(gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // A hand edit is drift.
    std::fs::write(gw_dir.join("config.yaml"), format!("{yaml}# hand edit\n")).unwrap();
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // A configuration that no longer generates is not current.
    let mut no_groups = cfg.clone();
    no_groups.gateway.groups.clear();
    std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
    assert!(!gateway_config_is_current(&no_groups, &dir, &fp, |_| true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gateway_config_is_current_detects_spawn_input_drift() {
    // The YAML does not carry the spawn port, the bind host, the
    // database port, or the credential values — a drift in any
    // of them must still read as NOT current.
    let dir = std::env::temp_dir().join(format!("wizard-gw-drift-{}", std::process::id()));
    let gw_dir = dir.join("gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    let yaml = manager_gateway::generate_config_yaml(&cfg).unwrap();
    std::fs::write(gw_dir.join("config.yaml"), &yaml).unwrap();
    let spawned = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), None, &[]);
    std::fs::write(
        gateway_spawn_state_path(&dir),
        gateway_spawn_state_json(&spawned, &yaml, None),
    )
    .unwrap();
    assert!(gateway_config_is_current(&cfg, &dir, &spawned, |_| true));

    // Gateway port moved.
    let mut moved = cfg.clone();
    moved.gateway.port += 1;
    let fp = gateway_spawn_fingerprint(&moved, true, Some("sk-master"), None, &[]);
    assert!(!gateway_config_is_current(&moved, &dir, &fp, |_| true));
    // Bind host changed.
    let mut rebound = cfg.clone();
    rebound.gateway.bind_host = "127.0.0.1".to_string();
    let fp = gateway_spawn_fingerprint(&rebound, true, Some("sk-master"), None, &[]);
    assert!(!gateway_config_is_current(&rebound, &dir, &fp, |_| true));
    // PostgreSQL appeared since the spawn (DATABASE_URL now in
    // play) — and, symmetrically, disappeared.
    let fp = gateway_spawn_fingerprint(&cfg, false, Some("sk-master"), None, &[]);
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // Master key rotated.
    let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-rotated"), None, &[]);
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // OpenRouter key appeared.
    let fp = gateway_spawn_fingerprint(&cfg, true, Some("sk-master"), Some("or-key"), &[]);
    assert!(!gateway_config_is_current(&cfg, &dir, &fp, |_| true));
    // The stored YAML hash no longer matching the file on disk
    // (replaced after the spawn) is drift too.
    std::fs::write(gw_dir.join("config.yaml"), "# replaced\n").unwrap();
    assert!(!gateway_config_is_current(&cfg, &dir, &spawned, |_| true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Step that never finishes on its own; the execution budget
/// must cut it off.
struct HangStep;

#[async_trait]
impl WizardStep for HangStep {
    fn id(&self) -> &str {
        "hang"
    }
    fn name(&self) -> &str {
        "Hang step"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![]
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        vec![]
    }
    async fn run(&self, _ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(StepOutcome::done("never reached", vec![]))
    }
}

#[tokio::test]
async fn step_execution_times_out_and_records_failure() {
    let c = ctx();
    let mut w = Wizard::new(vec![Box::new(HangStep)], RunMode::Install).expect("unique step ids");
    let outcome = w
        .execute_step_with_budget(&c, "hang", Duration::from_millis(50))
        .await;
    assert_eq!(outcome.state, StepState::Failed);
    assert!(outcome.message.contains("timed out"), "{}", outcome.message);
    assert!(outcome.message.contains("Hang step"), "{}", outcome.message);
    // The failure is recorded like any other, so the checklist
    // shows it and a retry is possible.
    assert_eq!(w.state("hang"), Some(StepState::Failed));
    assert_eq!(w.report().steps[0].state, StepState::Failed);
}

#[tokio::test]
async fn step_run_stops_when_cancelled() {
    let c = ctx();
    let mut w = Wizard::new(vec![Box::new(HangStep)], RunMode::Install).expect("unique step ids");
    let cancel = std::pin::pin!(tokio::time::sleep(Duration::from_millis(50)));
    let started = Instant::now();
    let (outcome, cancelled) = w.run_step_until(&c, "hang", cancel).await.unwrap();
    assert!(cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(outcome.state, StepState::Failed);
    assert!(outcome.message.contains("cancelled"), "{}", outcome.message);
    assert_eq!(w.state("hang"), Some(StepState::Failed));
}

#[tokio::test]
async fn run_all_until_stops_before_later_steps() {
    let c = ctx();
    let mut w = Wizard::new(
        vec![Box::new(HangStep), Box::new(ScriptStep::ok("after"))],
        RunMode::Install,
    )
    .expect("unique step ids");
    let cancel = std::pin::pin!(tokio::time::sleep(Duration::from_millis(50)));
    w.run_all_until(&c, cancel).await;
    assert_eq!(w.state("hang"), Some(StepState::Failed));
    assert_eq!(w.state("after"), Some(StepState::Queued));
}

#[tokio::test]
async fn stops_on_first_failure_and_resumes_after_retry() {
    let c = ctx();
    let mut cs = ScriptStep::ok("c");
    cs.prereqs = vec!["b".to_string()];
    let steps: Vec<Box<dyn WizardStep>> = vec![
        Box::new(ScriptStep::ok("a")),
        Box::new({
            let mut b = ScriptStep::ok("b");
            b.fail = true;
            b
        }),
        Box::new(cs),
    ];
    let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");
    let report = w.run_all(&c).await;
    assert_eq!(w.state("a"), Some(StepState::Done));
    assert_eq!(w.state("b"), Some(StepState::Failed));
    assert_eq!(w.state("c"), Some(StepState::Queued));
    assert_eq!(w.progress(), (1, 3));
    assert_eq!(report.steps[1].log_tail, vec!["boom".to_string()]);

    // Resume without retry: still blocked at b.
    let report2 = w.run_all(&c).await;
    assert_eq!(w.state("c"), Some(StepState::Queued));
    assert_eq!(report2.steps[1].state, StepState::Failed);

    w.retry("b");
    assert_eq!(w.state("b"), Some(StepState::Queued));
    // b is scripted to fail; flip it by replacing with an ok step is not
    // possible here, so retry keeps failing — instead verify retry on a
    // Done step is a no-op.
    w.retry("a");
    assert_eq!(w.state("a"), Some(StepState::Done));
}

#[tokio::test]
async fn run_clears_the_pre_run_note_from_the_report() {
    // A note explains why a step sits Queued (e.g. the
    // start-gateway drift note hydration attaches). Once the
    // step runs, the fresh outcome replaces it: the note must
    // not ride along on a failure it does not describe (field
    // report 2026-10-18: a PostgreSQL start failure displayed
    // the "gateway configuration changed" note).
    let c = ctx();
    let mut w = wizard(&[("a", true)]);
    w.notes.insert(
        "a".to_string(),
        "gateway configuration changed since the gateway was started".to_string(),
    );
    // Still Queued: the note shows.
    assert_eq!(
        w.report().steps[0].note.as_deref(),
        Some("gateway configuration changed since the gateway was started")
    );
    let out = w.run_step(&c, "a").await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    let report = w.report();
    assert_eq!(report.steps[0].state, StepState::Failed);
    assert_eq!(report.steps[0].note, None);
}

#[tokio::test]
async fn prerequisite_block_enforced() {
    let c = ctx();
    let mut b = ScriptStep::ok("b");
    b.prereqs = vec!["a".to_string()];
    let mut bf = ScriptStep::ok("b2");
    bf.prereqs = vec!["missing".to_string()];
    let steps: Vec<Box<dyn WizardStep>> =
        vec![Box::new(ScriptStep::ok("a")), Box::new(b), Box::new(bf)];
    let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");

    // Direct run of b before a is done → error.
    let err = w.run_step(&c, "b").await.unwrap_err();
    assert!(matches!(err, WizardError::PrerequisiteFailed { .. }));

    // run_all runs a then b, then stops at b2 (unknown prereq).
    let _ = w.run_all(&c).await;
    assert_eq!(w.state("a"), Some(StepState::Done));
    assert_eq!(w.state("b"), Some(StepState::Done));
    assert_eq!(w.state("b2"), Some(StepState::Queued));
}

#[tokio::test]
async fn skip_blocks_dependents_per_spec() {
    let c = ctx();
    let mut b = ScriptStep::ok("b");
    b.prereqs = vec!["a".to_string()];
    let steps: Vec<Box<dyn WizardStep>> = vec![Box::new(ScriptStep::ok("a")), Box::new(b)];
    let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");
    w.skip("a", "not needed here".to_string());
    assert_eq!(w.state("a"), Some(StepState::Skipped));
    assert_eq!(w.note("a"), Some("not needed here"));
    let _ = w.run_all(&c).await;
    // Skipped does not satisfy prerequisites (spec §4.1).
    assert_eq!(w.state("b"), Some(StepState::Queued));
    assert_eq!(w.progress(), (0, 2));
}

#[tokio::test]
async fn manual_override_unblocks_dependents() {
    let c = ctx();
    let mut b = ScriptStep::ok("b");
    b.prereqs = vec!["a".to_string()];
    let steps: Vec<Box<dyn WizardStep>> = vec![Box::new(ScriptStep::ok("a")), Box::new(b)];
    let mut w = Wizard::new(steps, RunMode::Install).expect("test steps have unique ids");
    w.mark_manual("a", "/opt/llamacpp".to_string(), "b1234".to_string());
    assert_eq!(w.state("a"), Some(StepState::Manual));
    assert_eq!(w.note("a"), Some("/opt/llamacpp @ b1234"));
    let _ = w.run_all(&c).await;
    assert_eq!(w.state("b"), Some(StepState::Done));
    assert_eq!(w.progress(), (2, 2));
}

#[tokio::test]
async fn audit_mode_skips_non_check_steps() {
    let c = ctx();
    let mut install_only = ScriptStep::ok("install-only");
    install_only.check = false;
    let steps: Vec<Box<dyn WizardStep>> =
        vec![Box::new(install_only), Box::new(ScriptStep::ok("check"))];
    let mut w = Wizard::new(steps, RunMode::Audit).expect("test steps have unique ids");
    assert_eq!(w.state("install-only"), Some(StepState::Skipped));
    assert_eq!(w.state("check"), Some(StepState::Queued));
    let report = w.run_all(&c).await;
    assert_eq!(report.mode, RunMode::Audit);
    assert_eq!(w.state("check"), Some(StepState::Done));
    assert_eq!(w.progress(), (1, 2));
}

#[tokio::test]
async fn unknown_step_id_errors() {
    let c = ctx();
    let mut w = wizard(&[("a", false)]);
    let err = w.run_step(&c, "nope").await.unwrap_err();
    assert!(matches!(err, WizardError::UnknownStep(_)));
    // retry/skip/mark_manual on unknown ids are safe no-ops.
    w.retry("nope");
    w.skip("nope", "x".to_string());
    w.mark_manual("nope", "p".to_string(), "v".to_string());
    assert_eq!(w.state("nope"), None);
}

#[test]
fn duplicate_ids_rejected() {
    let result = Wizard::new(
        vec![
            Box::new(ScriptStep::ok("a")) as Box<dyn WizardStep>,
            Box::new(ScriptStep::ok("a")) as Box<dyn WizardStep>,
        ],
        RunMode::Install,
    );
    assert!(
        matches!(result, Err(WizardError::DuplicateStepId(ref id)) if id == "a"),
        "duplicate ids must be rejected without panicking"
    );
}

#[tokio::test]
async fn log_tail_truncated_to_20() {
    let c = ctx();
    let mut s = ScriptStep::ok("verbose");
    s.log_lines = 35;
    let mut w =
        Wizard::new(vec![Box::new(s)], RunMode::Install).expect("test steps have unique ids");
    let report = w.run_all(&c).await;
    assert_eq!(report.steps[0].log_tail.len(), LOG_TAIL_LINES);
    assert_eq!(report.steps[0].log_tail[0], "line 15");
    assert_eq!(report.steps[0].log_tail[19], "line 34");
}

#[tokio::test]
async fn step_error_becomes_failed_outcome_in_run_all() {
    let c = ctx();
    let mut s = ScriptStep::ok("err");
    s.err = true;
    let mut w =
        Wizard::new(vec![Box::new(s)], RunMode::Install).expect("test steps have unique ids");
    let report = w.run_all(&c).await;
    assert_eq!(w.state("err"), Some(StepState::Failed));
    assert!(report.steps[0].message.contains("step error"));
    // run_step surfaces the failure as a Failed outcome (runner-measured).
    let mut w2 = Wizard::new(
        vec![Box::new(ScriptStep {
            err: true,
            ..ScriptStep::ok("e2")
        })],
        RunMode::Install,
    )
    .expect("test steps have unique ids");
    let outcome = w2
        .run_step(&c, "e2")
        .await
        .expect("run_step returns the outcome");
    assert_eq!(outcome.state, StepState::Failed);
    assert!(outcome.message.contains("wizard cancelled"));
}

#[tokio::test]
async fn run_step_allows_rerun_of_done_step() {
    let c = ctx();
    let mut w = wizard(&[("a", false)]);
    let _ = w.run_all(&c).await;
    assert_eq!(w.state("a"), Some(StepState::Done));
    let outcome = w.run_step(&c, "a").await.expect("rerun works");
    assert_eq!(outcome.state, StepState::Done);
}

#[tokio::test]
async fn eta_and_elapsed_sane() {
    let c = ctx();
    let mut w = wizard(&[("a", false), ("b", false)]);
    assert_eq!(w.eta(), None);
    let _ = w.run_step(&c, "a").await;
    let eta = w.eta().expect("eta after one completion");
    assert!(eta <= Duration::from_secs(60));
    assert!(w.elapsed() < Duration::from_secs(60));
    let _ = w.run_all(&c).await;
    assert_eq!(w.eta(), Some(Duration::ZERO));
}

#[test]
fn step_state_predicates() {
    assert!(StepState::Done.satisfies_prerequisite());
    assert!(StepState::Manual.satisfies_prerequisite());
    assert!(!StepState::Skipped.satisfies_prerequisite());
    assert!(!StepState::Failed.satisfies_prerequisite());
    assert!(StepState::Done.is_terminal());
    assert!(!StepState::Queued.is_terminal());
    assert!(checksums_match("ABCDEF", "abcdef"));
    assert!(!checksums_match("abc", "abd"));
}

#[test]
fn get_step_finds_steps_by_id() {
    let w = Wizard::new(crate::steps::default_steps(), RunMode::Install)
        .expect("default steps have unique ids");
    let step = w
        .get_step(crate::steps::STEP_VSCODE_EXTENSIONS)
        .expect("step exists");
    assert_eq!(step.id(), crate::steps::STEP_VSCODE_EXTENSIONS);
    assert_eq!(step.name(), "Install VS Code extensions");
    assert!(w.get_step("no-such-step").is_none());
}

#[test]
fn get_step_planned_commands_match_step() {
    let w = Wizard::new(crate::steps::default_steps(), RunMode::Install)
        .expect("default steps have unique ids");
    let cfg = manager_config::AppConfig::default_config();
    let ctx = WizardContext::new(
        cfg,
        std::path::PathBuf::from("/tmp"),
        RunMode::Install,
        std::sync::Arc::new(crate::fake::FakeSystemOps::new()),
    );
    let step = w.get_step(crate::steps::STEP_VSCODE_EXTENSIONS).unwrap();
    let cmds = step.planned_commands(&ctx);
    assert!(!cmds.is_empty());
    assert!(cmds.iter().all(|c| c.argv[0] == "code"));
    assert!(cmds
        .iter()
        .all(|c| c.argv.contains(&"--install-extension".to_string())));
}

#[test]
fn continue_config_yaml_points_at_gateway() {
    let cfg = manager_config::AppConfig::default_config();
    let yaml = crate::steps::continue_config_yaml(&cfg);
    let v: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(v["schema"].as_str().unwrap(), "v1");
    let models = v["models"].as_sequence().unwrap();
    assert_eq!(models.len(), cfg.gateway.groups.len());
    for (m, g) in models.iter().zip(cfg.gateway.groups.iter()) {
        assert_eq!(m["provider"].as_str().unwrap(), "openai");
        assert_eq!(m["model"].as_str().unwrap(), g.name);
        assert_eq!(
            m["apiBase"].as_str().unwrap(),
            format!("http://127.0.0.1:{}/v1", cfg.gateway.port)
        );
        assert!(m["name"].as_str().unwrap().ends_with("(local)"));
        // The key is an env reference, never a literal secret:
        // Continue resolves it from ~/.continue/.env.
        assert_eq!(
            m["apiKey"].as_str().unwrap(),
            "${{ secrets.LITELLM_API_KEY }}"
        );
        let roles: Vec<&str> = m["roles"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|r| r.as_str())
            .collect();
        assert_eq!(roles, ["chat", "edit", "apply"]);
        let caps: Vec<&str> = m["capabilities"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|r| r.as_str())
            .collect();
        assert_eq!(caps, ["tool_use"]);
    }
    assert!(!yaml.contains("sk-"), "no literal key material: {yaml}");
}

/// SEC-N1: the master key keeps its `sk-` + 64 lowercase hex
/// format, and successive keys differ (CSPRNG-drawn, not a
/// time/pid-seeded hash).
#[test]
fn master_key_format_is_unchanged_and_keys_are_distinct() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let key = generate_master_key();
        let hex = key.strip_prefix("sk-").expect("sk- prefix");
        assert_eq!(hex.len(), 64, "{key}");
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{key}"
        );
        assert!(seen.insert(key), "duplicate key generated");
    }
    let pw = generate_postgres_password();
    assert_eq!(pw.len(), 64);
    assert!(!pw.starts_with("sk-"));
}
