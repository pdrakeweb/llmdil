use super::*;

#[test]
fn in_flight_sums_labelled_and_plain_series() {
    let body = "\
# HELP litellm_in_flight_requests In flight
litellm_in_flight_requests{model=\"planner\"} 2.0
litellm_in_flight_requests 1.0
litellm_deployment_success_responses_total 9.0
";
    assert_eq!(parse_in_flight(body), Some(3.0));
    assert_eq!(parse_in_flight("litellm_other 1.0\n"), None);
}

#[test]
fn normalize_yaml_ignores_crlf_and_trailing_newlines() {
    assert_eq!(normalize_yaml("a\r\nb\r\n"), normalize_yaml("a\nb\n\n"));
}

fn entry(scope: &str, targets: &[&str]) -> ActivationEntry {
    ActivationEntry {
        id: String::new(),
        ts: "2026-10-16T12:00:00+00:00".to_string(),
        kind: "backend_flags".to_string(),
        summary: "change".to_string(),
        config_after: serde_json::Value::Null,
        scope: scope.to_string(),
        targets: targets.iter().map(|s| s.to_string()).collect(),
        pre_change_yaml_hash: None,
    }
}

fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

#[test]
fn backend_scope_resolves_on_restart_or_delete() {
    // No gateway YAML under this path, so gateway mtime never
    // resolves anything; backend scope resolves on start times.
    let dir = Path::new("/nonexistent-activation-test");
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![entry("backend", &["a", "b"])],
        ..Default::default()
    };
    let configured: std::collections::HashSet<String> =
        ["a".to_string(), "b".to_string()].into_iter().collect();
    let started = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), at(v)))
            .collect::<std::collections::HashMap<_, _>>()
    };
    // b has not restarted since the change: unresolved.
    let un = unresolved_entries_scoped(
        dir,
        &journal,
        &started(&[("a", "2026-10-16T13:00:00+00:00")]),
        &configured,
    );
    assert_eq!(un.len(), 1);
    // Both restarted after the entry: resolved.
    let un = unresolved_entries_scoped(
        dir,
        &journal,
        &started(&[
            ("a", "2026-10-16T13:00:00+00:00"),
            ("b", "2026-10-16T12:30:00+00:00"),
        ]),
        &configured,
    );
    assert!(un.is_empty());
    // A restart from BEFORE the entry does not resolve it.
    let un = unresolved_entries_scoped(
        dir,
        &journal,
        &started(&[
            ("a", "2026-10-16T11:00:00+00:00"),
            ("b", "2026-10-16T13:00:00+00:00"),
        ]),
        &configured,
    );
    assert_eq!(un.len(), 1);
    // A deleted target has nothing to activate: resolved when
    // the remaining target restarted.
    let configured: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
    let un = unresolved_entries_scoped(
        dir,
        &journal,
        &started(&[("a", "2026-10-16T13:00:00+00:00")]),
        &configured,
    );
    assert!(un.is_empty());
    // Gateway scope ignores start times entirely (mtime rule).
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![entry("gateway", &[])],
        ..Default::default()
    };
    let un = unresolved_entries_scoped(
        dir,
        &journal,
        &started(&[("a", "2026-10-16T13:00:00+00:00")]),
        &configured,
    );
    assert_eq!(un.len(), 1);
}

#[test]
fn record_and_cancel_use_scoped_resolution() {
    // Regression (code-review 4.1): record_change's prune and
    // cancel_target previously resolved with the gateway-only
    // rule, under which a backend-scope entry NEVER resolved —
    // a change already live via a backend restart stayed
    // "pending", and cancel would restore the snapshot over it.
    let dir = std::env::temp_dir().join(format!("activation-scoped-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = AppConfig::default_config();
    let journal = ActivationJournal {
        snapshot: Some(serde_json::to_value(&cfg).unwrap()),
        entries: vec![entry("backend", &["a"])],
        ..Default::default()
    };
    save_journal(&dir, &journal).unwrap();
    let configured: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
    // Backend "a" restarted at 13:00, after the 12:00 entry.
    let started: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
        [("a".to_string(), at("2026-10-16T13:00:00+00:00"))]
            .into_iter()
            .collect();
    // The entry is live: nothing to cancel.
    let running: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
    assert!(
        cancel_target(&dir, &cfg, &started, &configured, &running, true)
            .unwrap()
            .is_none()
    );
    // Recording a new change prunes the resolved entry and
    // restarts the snapshot from the new change's `pre`.
    let pre = cfg.clone();
    let mut post = cfg.clone();
    post.gateway.port += 1;
    record_change(
        &dir,
        &pre,
        &post,
        "gateway_config",
        "port".to_string(),
        "gateway",
        Vec::new(),
        &started,
        &configured,
        &running,
    )
    .unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].kind, "gateway_config");
    let snap: AppConfig =
        serde_json::from_value(journal.snapshot.expect("snapshot restarted")).unwrap();
    assert_eq!(snap.gateway.port, cfg.gateway.port);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_change_records_pre_change_yaml_hash() {
    let dir = std::env::temp_dir().join(format!("activation-yaml-hash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = AppConfig::default_config();
    let mut post = cfg.clone();
    post.gateway.port += 1;
    let started = std::collections::HashMap::new();
    let configured = std::collections::HashSet::new();
    let running = std::collections::HashSet::new();
    record_change(
        &dir,
        &cfg,
        &post,
        "gateway_config",
        "port".to_string(),
        "gateway",
        Vec::new(),
        &started,
        &configured,
        &running,
    )
    .unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    // No on-disk YAML in the fresh dir: the hash is of the YAML
    // generated from the pre-change config (when generation
    // succeeds for it).
    match manager_gateway::generate_config_yaml(&cfg) {
        Ok(yaml) => assert_eq!(
            journal.entries[0].pre_change_yaml_hash.as_deref(),
            Some(manager_wizard::gateway_yaml_hash(&yaml).as_str())
        ),
        Err(_) => assert!(journal.entries[0].pre_change_yaml_hash.is_none()),
    }
    // With an on-disk config.yaml present, its hash wins — it
    // is the YAML in effect, whatever the config now says.
    let yaml_path = gateway_yaml_path(&dir);
    std::fs::create_dir_all(yaml_path.parent().unwrap()).unwrap();
    std::fs::write(&yaml_path, "sentinel: true\n").unwrap();
    let mut post2 = post.clone();
    post2.gateway.port += 1;
    record_change(
        &dir,
        &post,
        &post2,
        "gateway_config",
        "port again".to_string(),
        "gateway",
        Vec::new(),
        &started,
        &configured,
        &running,
    )
    .unwrap();
    let journal = load_journal(&dir);
    let last = journal.entries.last().unwrap();
    assert_eq!(
        last.pre_change_yaml_hash.as_deref(),
        Some(manager_wizard::gateway_yaml_hash("sentinel: true\n").as_str())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_entry_without_scope_defaults_to_gateway() {
    let text = r#"{"ts":"2026-10-16T12:00:00+00:00","kind":"backend_add","summary":"x","config_after":null}"#;
    let e: ActivationEntry = serde_json::from_str(text).unwrap();
    assert_eq!(e.scope, "gateway");
    assert!(e.targets.is_empty());
    assert!(e.pre_change_yaml_hash.is_none());
}

fn ids(names: &[&str]) -> std::collections::HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn starts(
    pairs: &[(&str, &str)],
) -> std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> {
    pairs.iter().map(|(k, v)| (k.to_string(), at(v))).collect()
}

#[test]
fn stopped_backend_change_is_not_pending() {
    // Pete's report (2026-10-17): an offload edit on a stopped
    // backend produced a pending plan with no Apply. The entry
    // is strict-unresolved (the backend has never started since)
    // but already effective — nothing runs the old flags, and
    // the next start launches from the saved configuration.
    let dir = Path::new("/nonexistent-activation-test");
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![entry("backend", &["coder-nvidia"])],
        ..Default::default()
    };
    let configured = ids(&["coder-nvidia"]);
    let pending = pending_entries(dir, &journal, &starts(&[]), &configured, &ids(&[]), false);
    assert!(pending.is_empty());
    // The same for a gateway-scope entry while the gateway is
    // down: its next start regenerates the YAML from the saved
    // configuration, so the change is effective, not pending.
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![entry("gateway", &[])],
        ..Default::default()
    };
    let pending = pending_entries(dir, &journal, &starts(&[]), &configured, &ids(&[]), false);
    assert!(pending.is_empty());
    // ...but it IS pending while the gateway is up.
    let pending = pending_entries(dir, &journal, &starts(&[]), &configured, &ids(&[]), true);
    assert_eq!(pending.len(), 1);
}

#[test]
fn running_backend_change_is_pending() {
    let dir = Path::new("/nonexistent-activation-test");
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![entry("backend", &["coder"])],
        ..Default::default()
    };
    let configured = ids(&["coder"]);
    // Running, started BEFORE the 12:00 entry: the process
    // holds the pre-change state — pending (restart class).
    let pending = pending_entries(
        dir,
        &journal,
        &starts(&[("coder", "2026-10-16T11:00:00+00:00")]),
        &configured,
        &ids(&["coder"]),
        false,
    );
    assert_eq!(pending.len(), 1);
    // Running with NO known start (adopted): stays pending —
    // unknown uptime must never read as already effective.
    let pending = pending_entries(
        dir,
        &journal,
        &starts(&[]),
        &configured,
        &ids(&["coder"]),
        false,
    );
    assert_eq!(pending.len(), 1);
    // Running, (re)started AFTER the entry: the change is live
    // (the strict rule already resolved it) — not pending.
    let pending = pending_entries(
        dir,
        &journal,
        &starts(&[("coder", "2026-10-16T13:00:00+00:00")]),
        &configured,
        &ids(&["coder"]),
        false,
    );
    assert!(pending.is_empty());
}

#[test]
fn mixed_plan_only_the_running_targets_entry_is_pending() {
    let dir = Path::new("/nonexistent-activation-test");
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![
            entry("backend", &["coder-nvidia"]),
            entry("backend", &["coder"]),
        ],
        ..Default::default()
    };
    let configured = ids(&["coder-nvidia", "coder"]);
    let pending = pending_entries(
        dir,
        &journal,
        &starts(&[("coder", "2026-10-16T11:00:00+00:00")]),
        &configured,
        &ids(&["coder"]),
        false,
    );
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].targets, vec!["coder".to_string()]);
}

#[test]
fn next_start_of_stopped_backend_leaves_no_residue() {
    // The stopped backend's entry is already effective before
    // the start; after the start the strict rule resolves it,
    // and the next record_change prunes it from the journal —
    // no residue in the plan or the journal.
    let dir = std::env::temp_dir().join(format!(
        "activation-next-start-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = AppConfig::default_config();
    let journal = ActivationJournal {
        snapshot: Some(serde_json::to_value(&cfg).unwrap()),
        entries: vec![entry("backend", &["coder"])],
        ..Default::default()
    };
    save_journal(&dir, &journal).unwrap();
    let configured = ids(&["coder"]);
    // Before the start: not pending (already effective).
    assert!(
        pending_entries(&dir, &journal, &starts(&[]), &configured, &ids(&[]), false,).is_empty()
    );
    // After a start at 13:00 (entry is 12:00): strictly
    // resolved, and recording a further change prunes it.
    let started = starts(&[("coder", "2026-10-16T13:00:00+00:00")]);
    assert!(unresolved_entries_scoped(&dir, &journal, &started, &configured).is_empty());
    let mut post = cfg.clone();
    post.gateway.port += 1;
    record_change(
        &dir,
        &cfg,
        &post,
        "gateway_config",
        "port".to_string(),
        "gateway",
        Vec::new(),
        &started,
        &configured,
        &ids(&["coder"]),
    )
    .unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].kind, "gateway_config");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_change_prunes_an_already_effective_backend_entry() {
    // A change recorded for a stopped backend, then another:
    // the first entry prunes at the second record and the
    // snapshot restarts from the second change's `pre` — the
    // journal never accumulates already-effective entries.
    let dir = std::env::temp_dir().join(format!(
        "activation-effective-prune-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = AppConfig::default_config();
    let configured = ids(&["coder"]);
    let no_starts = starts(&[]);
    let nobody_running = ids(&[]);
    let mut post1 = cfg.clone();
    post1.gateway.port += 1;
    record_change(
        &dir,
        &cfg,
        &post1,
        "backend_flags",
        "first".to_string(),
        "backend",
        vec!["coder".to_string()],
        &no_starts,
        &configured,
        &nobody_running,
    )
    .unwrap();
    let mut post2 = post1.clone();
    post2.gateway.port += 1;
    record_change(
        &dir,
        &post1,
        &post2,
        "backend_flags",
        "second".to_string(),
        "backend",
        vec!["coder".to_string()],
        &no_starts,
        &configured,
        &nobody_running,
    )
    .unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].summary, "second");
    let snap: AppConfig =
        serde_json::from_value(journal.snapshot.expect("snapshot restarted")).unwrap();
    assert_eq!(snap.gateway.port, post1.gateway.port);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_uses_live_pending_state() {
    let dir = std::env::temp_dir().join(format!(
        "activation-cancel-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg0 = AppConfig::default_config();
    let mut cfg1 = cfg0.clone();
    cfg1.gateway.port += 1;
    let with_after = |scope: &str, targets: &[&str], after: &AppConfig| {
        let mut e = entry(scope, targets);
        e.config_after = serde_json::to_value(after).unwrap();
        e
    };
    let configured = ids(&["coder", "coder-nvidia"]);
    // Only an already-effective change (stopped target):
    // nothing to cancel — the edit stands (Pete's case).
    let journal = ActivationJournal {
        snapshot: Some(serde_json::to_value(&cfg0).unwrap()),
        entries: vec![with_after("backend", &["coder-nvidia"], &cfg1)],
        ..Default::default()
    };
    save_journal(&dir, &journal).unwrap();
    assert!(
        cancel_target(&dir, &cfg1, &starts(&[]), &configured, &ids(&[]), false,)
            .unwrap()
            .is_none()
    );
    // A genuinely pending change (running target, started
    // before the entry): cancel restores the snapshot.
    let journal = ActivationJournal {
        snapshot: Some(serde_json::to_value(&cfg0).unwrap()),
        entries: vec![with_after("backend", &["coder"], &cfg1)],
        ..Default::default()
    };
    save_journal(&dir, &journal).unwrap();
    let started = starts(&[("coder", "2026-10-16T11:00:00+00:00")]);
    let restored = cancel_target(&dir, &cfg1, &started, &configured, &ids(&["coder"]), false)
        .unwrap()
        .expect("pending change cancels");
    assert_eq!(restored.gateway.port, cfg0.gateway.port);
    // Pending + already-effective together: cancel refuses —
    // restoring the snapshot would discard the effective change
    // along with the pending one.
    let mut cfg2 = cfg1.clone();
    cfg2.gateway.port += 1;
    let journal = ActivationJournal {
        snapshot: Some(serde_json::to_value(&cfg0).unwrap()),
        entries: vec![
            with_after("backend", &["coder"], &cfg1),
            with_after("backend", &["coder-nvidia"], &cfg2),
        ],
        ..Default::default()
    };
    save_journal(&dir, &journal).unwrap();
    let err = cancel_target(&dir, &cfg2, &started, &configured, &ids(&["coder"]), false)
        .expect_err("mixed pending + effective must refuse");
    assert!(
        err.contains("already in effect"),
        "unexpected refusal: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn override_test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "activation-offload-override-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn offload_override_record_covers_round_trip() {
    let dir = override_test_dir("round-trip");
    record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
    let journal = load_journal(&dir);
    assert!(offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
    // Another backend at the same position is not covered.
    assert!(!offload_override_covers(&journal, "planner", 46, &[(6, 0)]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_override_wrong_layers_does_not_cover() {
    let dir = override_test_dir("wrong-layers");
    record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
    let journal = load_journal(&dir);
    assert!(!offload_override_covers(&journal, "coder", 45, &[(6, 0)]));
    assert!(!offload_override_covers(&journal, "coder", 48, &[(6, 0)]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_override_different_placed_cc_set_does_not_cover() {
    // A placement change invalidates the override: the position
    // was authorized against the placed GPUs it was saved on.
    let dir = override_test_dir("different-ccs");
    record_offload_override(&dir, "coder", 46, vec![(6, 0), (8, 6)]).unwrap();
    let journal = load_journal(&dir);
    // Order does not matter: the same set reversed still covers.
    assert!(offload_override_covers(
        &journal,
        "coder",
        46,
        &[(8, 6), (6, 0)]
    ));
    assert!(!offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
    assert!(!offload_override_covers(&journal, "coder", 46, &[(8, 6)]));
    assert!(!offload_override_covers(&journal, "coder", 46, &[]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_override_rerecord_replaces() {
    let dir = override_test_dir("rerecord");
    record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
    record_offload_override(&dir, "coder", 24, vec![(6, 0)]).unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.offload_overrides.len(), 1);
    assert!(!offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
    assert!(offload_override_covers(&journal, "coder", 24, &[(6, 0)]));
    // Clearing drops only that backend's records.
    record_offload_override(&dir, "planner", 10, vec![(6, 0)]).unwrap();
    clear_offload_overrides(&dir, "coder").unwrap();
    let journal = load_journal(&dir);
    assert!(!offload_override_covers(&journal, "coder", 24, &[(6, 0)]));
    assert!(offload_override_covers(&journal, "planner", 10, &[(6, 0)]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_journal_preserves_offload_overrides() {
    let dir = override_test_dir("clear-preserves");
    record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
    clear_journal(&dir).unwrap();
    let journal = load_journal(&dir);
    assert!(journal.entries.is_empty());
    assert!(journal.snapshot.is_none());
    assert!(offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_journal_without_overrides_field_loads_empty() {
    let dir = override_test_dir("legacy");
    std::fs::write(
        dir.join("activation-journal.json"),
        r#"{"snapshot":null,"entries":[]}"#,
    )
    .unwrap();
    let journal = load_journal(&dir);
    assert!(journal.offload_overrides.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-9 (docs/code-review-v0.1.60.md): a corrupt journal is moved
/// aside to `.corrupt-<unix-ms>` before the next write replaces
/// it, so its standing overrides are not silently erased.
#[test]
fn corrupt_journal_is_preserved_before_next_write() {
    let dir = override_test_dir("corrupt-preserved");
    let garbage = r#"{"snapshot":null,"entries":[ this is not json"#;
    std::fs::write(dir.join("activation-journal.json"), garbage).unwrap();
    let journal = load_journal(&dir);
    assert!(journal.entries.is_empty());
    assert!(journal.offload_overrides.is_empty());
    record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
    let preserved: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("activation-journal.json.corrupt-"))
        })
        .collect();
    assert_eq!(preserved.len(), 1, "{preserved:?}");
    assert_eq!(std::fs::read_to_string(&preserved[0]).unwrap(), garbage);
    let journal = load_journal(&dir);
    assert!(offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
    let _ = std::fs::remove_dir_all(&dir);
}

fn drift_test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("activation-drift-{label}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("gateway")).unwrap();
    dir
}

#[test]
fn config_drift_ignores_an_unarmed_gateway_mcp_server() {
    // Regression (tracker #42): the start-gateway step writes
    // the YAML from the armed-only MCP view, so a Gateway
    // server whose credential does not resolve is omitted from
    // the file. Drift generated from the raw registry carried
    // that server's mcp_servers block forever: every plan —
    // including backend-only ones — demanded a gateway restart
    // no restart could satisfy. The default config's curated
    // seed is exactly the production case: brave_search is
    // Gateway-exposed with an env key whose credential most
    // installs have not stored.
    let dir = drift_test_dir("unarmed");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    assert!(cfg
        .mcp
        .servers
        .iter()
        .any(|s| s.exposure == manager_config::McpExposure::Gateway && !s.env_keys.is_empty()));
    // The file a start writes while the server is unarmed.
    let unarmed = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&unarmed).unwrap();
    assert!(!yaml.contains("mcp_servers:"), "{yaml}");
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    assert!(!config_drift(&cfg, &dir, |_| false));
    // The same file reads as drift once the credential
    // resolves: a start now would write the server in.
    assert!(config_drift(&cfg, &dir, |_| true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_drift_detects_a_real_gateway_change() {
    let dir = drift_test_dir("real-change");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    let armed = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| true);
    let yaml = manager_gateway::generate_config_yaml(&armed).unwrap();
    assert!(yaml.contains("mcp_servers:"), "{yaml}");
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    assert!(!config_drift(&cfg, &dir, |_| true));
    // A saved change the running gateway has not picked up is
    // still drift: a backend's port is carried in the YAML as
    // the deployment's api_base. (The gateway's own listen
    // port is not — it travels via the spawn argv.)
    cfg.backends[0].port += 1;
    assert!(config_drift(&cfg, &dir, |_| true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_drift_covers_the_routing_callback_file() {
    // A stale llmdil_routing.py beside an identical YAML is
    // drift (the wizard's check has always compared it); a
    // callback file with no enabled rule is drift too.
    let dir = drift_test_dir("routing");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    // The file a start writes: the armed-only generation.
    let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    assert!(!config_drift(&cfg, &dir, |_| false));
    std::fs::write(gateway_routing_path(&dir), "# stale callback\n").unwrap();
    assert!(config_drift(&cfg, &dir, |_| false));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plan_drift_is_pending_only_while_the_gateway_is_up() {
    // The raw file comparison is true whenever the on-disk
    // YAML is stale; as PLAN state it counts only while a
    // running gateway serves that file. Down, the next start
    // regenerates it — nothing is pending and nothing may be
    // stated as the plan's reason (tracker #45).
    let dir = drift_test_dir("plan-gate");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    cfg.backends[0].port += 1;
    let raw = config_drift(&cfg, &dir, |_| false);
    assert!(raw, "the stale file is raw drift");
    assert!(!drift_is_pending(raw, false));
    assert!(drift_is_pending(raw, true));
    assert!(!drift_is_pending(false, true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backend_only_plan_with_a_stale_gateway_file_reports_no_gateway_drift() {
    // Regression (tracker #45), the production shape: one
    // backend-scope journal entry (a coder flags save — flags
    // are not carried in the gateway YAML, so no gateway entry
    // exists), the gateway DOWN (Pete's start had failed), and
    // an on-disk gateway YAML left stale by an earlier start.
    // The plan built from this state must show exactly the
    // backend change and no gateway drift: the raw drift bit
    // leaked into the plan payload and the card asserted
    // "Gateway config: drifted" above "Apply restarts coder.",
    // a restart that neither addresses nor clears any drift.
    let dir = drift_test_dir("backend-only-plan");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    // The file an earlier start wrote (coder on its old port).
    let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    // Journal the backend-only change: coder's flags, with
    // coder running a process started before the save.
    let pre = cfg.clone();
    let mut post = cfg.clone();
    post.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .raw_flags = Some("-ot exps=CPU".to_string());
    let configured: std::collections::HashSet<String> =
        post.backends.iter().map(|b| b.id.clone()).collect();
    let running: std::collections::HashSet<String> = ["coder".to_string()].into_iter().collect();
    let started_at: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> = [(
        "coder".to_string(),
        chrono::Utc::now() - chrono::Duration::seconds(60),
    )]
    .into_iter()
    .collect();
    record_change(
        &dir,
        &pre,
        &post,
        "backend_flags",
        "Backend `coder` flags changed".to_string(),
        "backend",
        vec!["coder".to_string()],
        &started_at,
        &configured,
        &running,
    )
    .unwrap();
    // The on-disk file predates the current configuration (a
    // port move since the start that wrote it), so the raw
    // comparison is drift — the state Pete's plan read.
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .port += 1;
    let raw = config_drift(&cfg, &dir, |_| false);
    assert!(raw, "precondition: the stale file is raw drift");
    // The plan's composition (op_gateway_activation_plan):
    // pending entries plus drift gated on the gateway probe.
    let journal = load_journal(&dir);
    let pending = pending_entries(&dir, &journal, &started_at, &configured, &running, false);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].scope, "backend");
    assert_eq!(pending[0].targets, vec!["coder".to_string()]);
    assert!(!pending[0].summary.to_lowercase().contains("gateway"));
    assert!(
        !drift_is_pending(raw, false),
        "a backend-only plan must not report gateway drift while the gateway is down"
    );
    // The same stale file IS plan drift once the gateway is up
    // serving it: then the plan owes a gateway restart, and
    // gateway_pending (hence the apply line) must say so.
    assert!(drift_is_pending(raw, true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_drift_detail_names_the_differing_element() {
    // Tracker #57: the plan's drift item must say WHICH
    // generated file differs, so the element-level comparison
    // reports the YAML and the routing callback separately.
    let dir = drift_test_dir("detail");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    let detail = config_drift_detail(&cfg, &dir, |_| false);
    assert_eq!(
        detail,
        DriftDetail {
            yaml: false,
            routing: false
        }
    );
    assert!(!detail.any());
    // A callback file no enabled rule justifies: routing only.
    std::fs::write(gateway_routing_path(&dir), "# stale callback\n").unwrap();
    let detail = config_drift_detail(&cfg, &dir, |_| false);
    assert_eq!(
        detail,
        DriftDetail {
            yaml: false,
            routing: true
        }
    );
    // A saved change the YAML carries: YAML too.
    std::fs::remove_file(gateway_routing_path(&dir)).unwrap();
    cfg.backends[0].port += 1;
    let detail = config_drift_detail(&cfg, &dir, |_| false);
    assert_eq!(
        detail,
        DriftDetail {
            yaml: true,
            routing: false
        }
    );
    assert!(detail.any());
    assert_eq!(config_drift(&cfg, &dir, |_| false), detail.any());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drift_plan_change_item_shape_and_summary() {
    // The synthesized item carries the fields every plan
    // surface renders (ts/kind/summary/scope/targets/
    // activation), classifies as a gateway restart, and names
    // exactly the differing element(s) — never a cause.
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-18T22:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let item = drift_plan_change(
        &DriftDetail {
            yaml: true,
            routing: false,
        },
        Some(at),
    );
    assert_eq!(item["kind"], "gateway_drift");
    assert_eq!(item["scope"], "gateway");
    assert_eq!(item["activation"], "gateway_restart");
    assert_eq!(item["targets"], serde_json::json!([]));
    assert_eq!(item["ts"], "2026-10-18T22:00:00+00:00");
    let summary = item["summary"].as_str().unwrap();
    assert!(summary.contains("config.yaml"), "{summary}");
    assert!(!summary.contains("llmdil_routing.py"), "{summary}");

    let item = drift_plan_change(
        &DriftDetail {
            yaml: false,
            routing: true,
        },
        None,
    );
    let summary = item["summary"].as_str().unwrap();
    assert!(summary.contains("llmdil_routing.py"), "{summary}");
    assert!(!summary.contains("config.yaml"), "{summary}");

    let item = drift_plan_change(
        &DriftDetail {
            yaml: true,
            routing: true,
        },
        None,
    );
    let summary = item["summary"].as_str().unwrap();
    assert!(summary.contains("config.yaml"), "{summary}");
    assert!(summary.contains("llmdil_routing.py"), "{summary}");
}

#[test]
fn drift_only_state_composes_a_one_item_plan() {
    // Regression (tracker #57), Pete's production shape: the
    // gateway is UP serving files an earlier start wrote; the
    // saved state has since drifted with NO journal entry —
    // here the journal-free trigger itself: an MCP credential
    // stored after the start arms a Gateway server, so a start
    // now would write an mcp_servers block the on-disk file
    // lacks (the #42 fixture's armed side). The plan composed
    // the way op_gateway_activation_plan composes it must
    // contain exactly one change — the drift item — so no
    // surface can render "Pending changes: 0" beside an Apply.
    let dir = drift_test_dir("drift-only-plan");
    let mut cfg = AppConfig::default_config();
    for b in cfg.backends.iter_mut() {
        b.enabled = true;
    }
    // The file the running gateway was started with: written
    // while the seeded Gateway server was unarmed.
    let unarmed = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&unarmed).unwrap();
    std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
    // No journal file at all: nothing was ever recorded.
    let journal = load_journal(&dir);
    let started_at = std::collections::HashMap::new();
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let running = std::collections::HashSet::new();
    let pending = pending_entries(&dir, &journal, &started_at, &configured, &running, true);
    assert!(pending.is_empty(), "precondition: no journal entries");
    // The credential now resolves: drift, gateway up.
    let detail = config_drift_detail(&cfg, &dir, |_| true);
    assert!(detail.any(), "precondition: the armed view drifted");
    assert!(detail.yaml);
    let drift = drift_is_pending(detail.any(), true);
    assert!(drift);
    // The op's composition: entry changes (none) plus the
    // synthesized drift item.
    let mut changes: Vec<serde_json::Value> = Vec::new();
    if drift {
        changes.push(drift_plan_change(&detail, gateway_config_written_at(&dir)));
    }
    assert_eq!(changes.len(), 1, "a pending plan never has zero items");
    assert_eq!(changes[0]["kind"], "gateway_drift");
    assert_eq!(changes[0]["activation"], "gateway_restart");
    // The same state with the gateway DOWN composes no item:
    // the stale file is information, not pending work (#45).
    assert!(!drift_is_pending(detail.any(), false));
    let _ = std::fs::remove_dir_all(&dir);
}

fn ps_test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "activation-ps-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn record_gateway(dir: &Path, pre: &AppConfig, post: &AppConfig, summary: &str) -> String {
    record_change(
        dir,
        pre,
        post,
        "gateway_config",
        summary.to_string(),
        "gateway",
        Vec::new(),
        &starts(&[]),
        &ids(&[]),
        &ids(&[]),
    )
    .unwrap()
}

#[test]
fn rollback_removes_only_its_entry_and_keeps_a_concurrent_override() {
    // PS-6: a command records, a concurrent command records an
    // offload override, then the first command's save fails.
    // The rollback must remove exactly the first command's entry
    // — not pop-last, not a restore of the pre-record journal —
    // so the override and an earlier entry survive.
    let dir = ps_test_dir("rollback");
    let cfg0 = AppConfig::default_config();
    let mut cfg1 = cfg0.clone();
    cfg1.gateway.port += 1;
    let mut cfg2 = cfg1.clone();
    cfg2.gateway.port += 1;
    let earlier = record_gateway(&dir, &cfg0, &cfg1, "earlier");
    let failed = record_gateway(&dir, &cfg1, &cfg2, "failed save");
    assert_ne!(earlier, failed, "entry ids are unique");
    record_offload_override(&dir, "coder", 24, vec![(8, 9)]).unwrap();
    JournalRecords::one(failed.clone()).roll_back(&dir).unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].id, earlier);
    assert_eq!(journal.entries[0].summary, "earlier");
    assert!(
        journal.snapshot.is_some(),
        "the earlier entry keeps its cancel target"
    );
    assert!(offload_override_covers(&journal, "coder", 24, &[(8, 9)]));
    // Rolling back the last entry empties the journal and drops
    // the snapshot; the override still stands.
    JournalRecords::one(earlier).roll_back(&dir).unwrap();
    let journal = load_journal(&dir);
    assert!(journal.entries.is_empty());
    assert!(journal.snapshot.is_none());
    assert_eq!(journal.offload_overrides.len(), 1);
    // An unknown id (already cleared) and an empty id are no-ops.
    remove_entries(&dir, &["gone".to_string(), String::new()]).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_entries_without_ids_load_and_are_never_rolled_back() {
    let dir = ps_test_dir("legacy-id");
    let cfg = AppConfig::default_config();
    let legacy = serde_json::json!({
        "snapshot": serde_json::to_value(&cfg).unwrap(),
        "entries": [{
            "ts": "2026-10-16T12:00:00+00:00",
            "kind": "gateway_config",
            "summary": "legacy",
            "config_after": serde_json::to_value(&cfg).unwrap(),
        }],
    });
    std::fs::write(journal_path(&dir), legacy.to_string()).unwrap();
    let journal = load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert!(journal.entries[0].id.is_empty());
    remove_entries(&dir, &[String::new()]).unwrap();
    assert_eq!(load_journal(&dir).entries.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A journal with one pending backend-scope entry (`coder`
/// running, started before the entry) from `snapshot` to `after`.
fn pending_journal(dir: &Path, snapshot: &AppConfig, after: &AppConfig) {
    let mut e = entry("backend", &["coder"]);
    e.config_after = serde_json::to_value(after).unwrap();
    save_journal(
        dir,
        &ActivationJournal {
            snapshot: Some(serde_json::to_value(snapshot).unwrap()),
            entries: vec![e],
            ..Default::default()
        },
    )
    .unwrap();
}

fn plan(dir: &Path, current: &AppConfig) -> Result<CancelPlan, String> {
    cancel_plan(
        dir,
        current,
        &starts(&[("coder", "2026-10-16T11:00:00+00:00")]),
        &ids(&["coder"]),
        &ids(&["coder"]),
        false,
    )
}

#[test]
fn cancel_ignores_background_stamps_and_keeps_them_on_restore() {
    // PS-10: a download verification stamp and the wizard's
    // setup_completed landed after the change was journaled. The
    // guard used to refuse forever; now they are bookkeeping —
    // ignored by the comparison and carried into the restore.
    let dir = ps_test_dir("cancel-stamps");
    let cfg0 = crate::activation_cancel::test_config_with_model();
    let mut cfg1 = cfg0.clone();
    cfg1.gateway.port += 1;
    pending_journal(&dir, &cfg0, &cfg1);
    let mut current = cfg1.clone();
    let stamp = chrono::Utc::now();
    current.models[0].verified_at = Some(stamp);
    current.models[0].verified_sha256 = Some(current.models[0].sha256.clone());
    current.setup_completed = !cfg0.setup_completed;
    let CancelPlan::Restore(restored) = plan(&dir, &current).unwrap() else {
        panic!("a bookkeeping-only difference must not refuse cancel");
    };
    assert_eq!(restored.gateway.port, cfg0.gateway.port);
    assert_eq!(restored.models[0].verified_at, Some(stamp));
    assert_eq!(restored.setup_completed, current.setup_completed);
    // A real configuration difference still refuses.
    current.telemetry = {
        let mut t = current.telemetry.clone();
        t.poll_interval_ms += 1;
        t
    };
    let err = plan(&dir, &current).expect_err("an operator change refuses");
    assert!(err.contains("another path"), "unexpected refusal: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_refuses_to_roll_back_a_security_section() {
    // The snapshot predates an admin API change that reached the
    // current configuration (e.g. a non-journaled save between
    // two journaled ones): restoring would roll it back.
    let dir = ps_test_dir("cancel-security");
    let cfg0 = AppConfig::default_config();
    let mut cfg1 = cfg0.clone();
    cfg1.gateway.port += 1;
    cfg1.admin_api.port = cfg1.admin_api.port.wrapping_add(1);
    pending_journal(&dir, &cfg0, &cfg1);
    let err = plan(&dir, &cfg1).expect_err("security rollback refuses");
    assert!(err.contains("admin_api"), "names the section: {err}");
    assert!(err.contains("security"), "says why: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_self_heals_a_journal_left_after_a_restore() {
    // Crash between a cancel's config save and its journal
    // clear: the configuration equals the snapshot while the
    // entry still says otherwise. Cancel reports the stale
    // journal (the op clears it) instead of refusing forever.
    let dir = ps_test_dir("cancel-heal");
    let cfg0 = crate::activation_cancel::test_config_with_model();
    let mut cfg1 = cfg0.clone();
    cfg1.gateway.port += 1;
    pending_journal(&dir, &cfg0, &cfg1);
    let mut current = cfg0.clone();
    current.models[0].verified_at = Some(chrono::Utc::now());
    assert!(matches!(
        plan(&dir, &current).unwrap(),
        CancelPlan::StaleJournal
    ));
    // A no-op change (after == snapshot) is not mistaken for a
    // stale journal.
    pending_journal(&dir, &cfg0, &cfg0);
    assert!(matches!(plan(&dir, &cfg0).unwrap(), CancelPlan::Restore(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gateway_entries_resolve_from_the_applied_stamp_not_the_yaml() {
    // CR-1: a failed restart rewrites the YAML but writes no
    // stamp, so an entry newer than the last successful start
    // stays unresolved even though the YAML is newer still.
    let dir = drift_test_dir("applied-stamp");
    let _ = std::fs::remove_file(manager_wizard::gateway_applied_stamp_path(&dir));
    std::fs::write(gateway_yaml_path(&dir), "model_list: []\n").unwrap();
    let journal = ActivationJournal {
        snapshot: None,
        entries: vec![entry("gateway", &[])],
        ..Default::default()
    };
    let none = std::collections::HashMap::new();
    let configured = std::collections::HashSet::new();
    // No stamp: the YAML mtime (now) resolves the 2026-10-16 entry.
    assert!(unresolved_entries_scoped(&dir, &journal, &none, &configured).is_empty());
    // A stamp from before the entry keeps it pending.
    manager_wizard::write_gateway_applied_stamp(&dir, at("2026-10-16T11:00:00+00:00"), 4).unwrap();
    assert_eq!(
        gateway_config_written_at(&dir),
        Some(at("2026-10-16T11:00:00+00:00"))
    );
    assert_eq!(
        unresolved_entries_scoped(&dir, &journal, &none, &configured).len(),
        1
    );
    // A successful start after the entry resolves it.
    manager_wizard::write_gateway_applied_stamp(&dir, at("2026-10-16T12:00:01+00:00"), 5).unwrap();
    assert!(unresolved_entries_scoped(&dir, &journal, &none, &configured).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
